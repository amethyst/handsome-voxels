use super::{input::ProcessedInput, ThirdPersonCameraState, ThirdPersonControlConfig};

use voxel_mapper::{
    collision::{
        earliest_toi, extreme_ball_voxel_impact, floor_translation::translate_over_floor, VoxelBVT,
    },
    geometry::{project_point_onto_line, Line, UP},
    voxel::{
        search::{
            find_path_through_voxels_with_l1_and_linear_heuristic,
            find_path_through_voxels_with_l1_heuristic,
        },
        voxel_containing_point, voxel_is_empty, LatPoint3, VoxelMap,
    },
};

use amethyst::core::math::{Point3, Vector3};
use ilattice3 as lat;
use ncollide3d::query::TOI;
use serde::{Deserialize, Serialize};

#[cfg(feature = "profiler")]
use thread_profiler::profile_scope;

/// Constant parameters for tuning the camera collision controller.
#[derive(Deserialize, Serialize)]
pub struct CameraCollisionConfig {
    /// Size of the collidable ball surrounding the camera.
    ball_radius: f32,
    /// The smallest orthogonal deviation from the eye line that's considered a significant
    /// obstruction.
    min_obstruction_width: f32,
    /// The minimum distance of a range along the eye line. This prevents small ranges that are too
    /// small to give the camera sphere enough room to move.
    min_range_length: f32,
    /// The cutoff distance below which we don't event try doing a camera search.
    not_worth_searching_dist: f32,
    /// The maximum number of A* iterations we will do in the camera search. This is important so
    /// the search stops in a reasonable time if it can't connect with the camera.
    max_search_iterations: usize,
    /// When projecting a point on the search path onto the eye line, we need to make sure it's
    /// still path-connected to the same empty space (to avoid going through solid boundaries). We
    /// use another A* search to determine the connectivity, and this is the max # of iterations.
    projection_connection_max_iterations: usize,
    /// If the distance to the camera target falls below this threshold, the camera locks into a
    /// fixed distance from the target.
    camera_lock_threshold: f32,
    /// When the camera is locked to a fixed distance from the target, this is that distance.
    camera_lock_radius: f32,
    /// Used to as the offset from the end of a path range for choosing where to start a sphere cast
    /// inside that range. The hope is that we won't choose a point so close to the end of the range
    /// that the sphere is immediately colliding with something.
    range_point_selection_offset: usize,
}

/// Resolves collisions to prevent occluding the target.
pub struct CollidingController {
    colliding: bool,
    last_empty_feet_point: Option<lat::Point>,
    previous_camera_voxel: Option<lat::Point>,
}

impl CollidingController {
    pub fn new() -> Self {
        Self {
            colliding: false,
            last_empty_feet_point: None,
            previous_camera_voxel: None,
        }
    }

    pub fn apply_input(
        &mut self,
        config: &ThirdPersonControlConfig,
        mut cam_state: ThirdPersonCameraState,
        input: &ProcessedInput,
        voxel_map: &VoxelMap,
        voxel_bvt: &VoxelBVT,
    ) -> ThirdPersonCameraState {
        // Figure out the where the camera feet are.
        cam_state.feet = translate_over_floor(
            &cam_state.feet,
            &input.feet_translation,
            &voxel_map.voxels,
            true,
        );
        // Figure out where the camera target is.
        cam_state.target = cam_state.feet + config.target_height_above_feet * Vector3::from(UP);

        self.set_desired_camera_position(input, config, &mut cam_state);

        let voxel_is_empty_fn = |p: &lat::Point| voxel_is_empty(&voxel_map.voxels, p);
        self.resolve_camera_collisions(
            &config.collision,
            &voxel_is_empty_fn,
            voxel_bvt,
            &mut cam_state,
        );

        cam_state
    }

    fn set_desired_camera_position(
        &self,
        input: &ProcessedInput,
        config: &ThirdPersonControlConfig,
        cam_state: &mut ThirdPersonCameraState,
    ) {
        // Rotate around the target.
        cam_state.add_yaw(input.delta_yaw);
        cam_state.add_pitch(input.delta_pitch);

        // Scale the camera's distance from the target.
        if input.radius_scalar > 1.0 {
            // Don't move the camera if it's colliding.
            if !self.colliding {
                cam_state.scale_radius(input.radius_scalar, config);
            }
        } else if input.radius_scalar < 1.0 {
            // If the desired radius is longer than actual because of collision, just truncate it
            // so the camera moves as soon as the player starts shortening the radius.
            cam_state.set_radius(cam_state.get_actual_radius(), config);

            cam_state.scale_radius(input.radius_scalar, config);
        }
    }

    fn resolve_camera_collisions(
        &mut self,
        config: &CameraCollisionConfig,
        voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
        voxel_bvt: &VoxelBVT,
        cam_state: &mut ThirdPersonCameraState,
    ) {
        let desired_position = cam_state.get_desired_position();

        // Choose an empty voxel to start our search path.
        let feet_voxel = voxel_containing_point(&cam_state.feet);
        self.set_last_empty_feet_voxel(voxel_is_empty_fn, feet_voxel);
        let empty_path_start = self.last_empty_feet_point.clone().unwrap();

        // We always try to find a short path around voxels that occlude the target before doing
        // the sphere cast.
        let sphere_cast_start = self.find_start_of_sphere_cast(
            &empty_path_start,
            cam_state.target,
            desired_position,
            voxel_is_empty_fn,
            config,
        );
        let (was_collision, camera_after_collisions) = move_ball_until_collision(
            config.ball_radius,
            &sphere_cast_start,
            &desired_position,
            voxel_bvt,
            earliest_toi,
            |_| true,
        );
        self.colliding = was_collision;

        if (camera_after_collisions - cam_state.target).norm_squared()
            < config.camera_lock_threshold.powi(2)
        {
            // If we're really close to the target, wonky stuff can happen with collisions, so just
            // lock into a tight sphere.
            cam_state.actual_position = cam_state.get_position_at_radius(config.camera_lock_radius);
        } else {
            cam_state.actual_position = camera_after_collisions;
        }

        self.previous_camera_voxel = Some(voxel_containing_point(&cam_state.actual_position));
    }

    /// Try to find the ideal location to cast a sphere from.
    fn find_start_of_sphere_cast(
        &mut self,
        path_start: &lat::Point,
        target: Point3<f32>,
        camera: Point3<f32>,
        voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
        config: &CameraCollisionConfig,
    ) -> Point3<f32> {
        // If we want to be close to the camera, there's not much use in finding a path around
        // occluders.
        if (target - camera).norm_squared() < config.not_worth_searching_dist.powi(2) {
            return target;
        }

        #[cfg(feature = "profiler")]
        profile_scope!("find_start_of_sphere_cast");

        let eye_ray = Line::from_endpoints(target, camera);

        // Graph search away from the target to get as close to the camera as possible. It's OK if
        // we don't reach the camera, since we'll still return the path that got closest.
        let path_finish = voxel_containing_point(&camera);
        let (_reached_finish, path) = find_path_through_voxels_with_l1_and_linear_heuristic(
            path_start,
            &path_finish,
            voxel_is_empty_fn,
            config.max_search_iterations,
        );

        let unobstructed_ranges =
            find_unobstructed_ranges(&path, &eye_ray, voxel_is_empty_fn, config);

        self.find_start_of_sphere_cast_in_ranges(
            &unobstructed_ranges,
            &path,
            &eye_ray,
            voxel_is_empty_fn,
            config,
        )
        .unwrap_or(target)
    }

    fn find_start_of_sphere_cast_in_ranges(
        &self,
        unobstructed_ranges: &[([usize; 2], [f32; 2])],
        path: &[lat::Point],
        eye_line: &Line,
        voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
        config: &CameraCollisionConfig,
    ) -> Option<Point3<f32>> {
        let mut best_point = None;
        let mut best_point_closeness = std::usize::MAX;

        // If the camera is really close to the target, it might not show up in the unobstructed
        // ranges, due to min_range_length, but we still want to consider it.
        if let Some(target_point) = self.last_empty_feet_point {
            let target_closeness =
                if let Some(previous_camera) = self.previous_camera_voxel.as_ref() {
                    let (_reached_finish, path) = find_path_through_voxels_with_l1_heuristic(
                        &target_point,
                        previous_camera,
                        voxel_is_empty_fn,
                        config.max_search_iterations,
                    );

                    path.len()
                } else {
                    std::usize::MAX
                };

            best_point = Some(target_point);
            best_point_closeness = target_closeness;
        }

        for (range, _) in unobstructed_ranges.iter() {
            let point_in_range = find_start_of_sphere_cast_in_range(
                path,
                range,
                config.range_point_selection_offset,
            );
            let closeness = if let Some(previous_camera) = self.previous_camera_voxel.as_ref() {
                let (_reached_finish, path) = find_path_through_voxels_with_l1_heuristic(
                    &point_in_range,
                    previous_camera,
                    voxel_is_empty_fn,
                    config.max_search_iterations,
                );

                path.len()
            } else {
                0
            };
            // Must be closer than the current best point.
            if closeness < best_point_closeness {
                best_point_closeness = closeness;
                best_point = Some(point_in_range);
            }
        }

        best_point.map(|p| {
            let LatPoint3(p) = p.into();

            project_point_onto_line(&p, eye_line)
        })
    }

    fn set_last_empty_feet_voxel(
        &mut self,
        voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
        new_feet: lat::Point,
    ) {
        // HACK: really, the feet should never be in a non-empty voxel
        if self.last_empty_feet_point.is_some() {
            if voxel_is_empty_fn(&new_feet) {
                self.last_empty_feet_point = Some(new_feet);
            }
        } else {
            self.last_empty_feet_point = Some(new_feet);
        }
    }
}

/// Choose the point in the range that has the best chance of casting the sphere farthest, i.e. a
/// point that's close to the end of the range, but not too close.
fn find_start_of_sphere_cast_in_range(
    path: &[lat::Point],
    path_range: &[usize; 2],
    selection_offset: usize,
) -> lat::Point {
    let chosen_index = if path_range[1] - path_range[0] > selection_offset {
        path_range[1] - selection_offset
    } else {
        path_range[0]
    };

    path[chosen_index]
}

/// Given `path`, find all contiguous ranges of points that are not obstructed by non-empty voxels.
/// The ranges are open-ended, i.e. not including the end: [start, end).
fn find_unobstructed_ranges(
    path: &[lat::Point],
    eye_line: &Line,
    voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
    config: &CameraCollisionConfig,
) -> Vec<([usize; 2], [f32; 2])> {
    let mut unobstructed_ranges = Vec::new();
    let mut current_range_start = Some((0, 0.0));

    let mut try_add_range =
        |end_index: usize, p_proj: Point3<f32>, range_start: &mut Option<(usize, f32)>| {
            if let Some((start_index, start_dist)) = *range_start {
                if end_index > start_index {
                    let end_dist = (p_proj - eye_line.p).norm();
                    if end_dist - start_dist > config.min_range_length {
                        unobstructed_ranges.push(([start_index, end_index], [start_dist, end_dist]))
                    }
                }

                *range_start = None;
            }
        };

    for (i, p) in path.iter().enumerate() {
        let LatPoint3(p_float) = (*p).into();
        let p_proj = project_point_onto_line(&p_float, &eye_line);

        if point_is_obstructed(p, &p_float, &p_proj, voxel_is_empty_fn, config) {
            try_add_range(i, p_proj, &mut current_range_start);
        } else if let None = current_range_start {
            // We're no longer obstructed, so start a new range.
            current_range_start = Some((i, (p_proj - eye_line.p).norm()));
        }
    }

    // Finish off the last range.
    if let Some(p) = path.last() {
        let LatPoint3(p_float) = (*p).into();
        try_add_range(
            path.len(),
            project_point_onto_line(&p_float, &eye_line),
            &mut current_range_start,
        );
    }

    unobstructed_ranges
}

/// Figure out if point p is obstructed, which is the case if:
///   1. The point strays too far from the eye line.
///   2. The voxel projection of the point on the eye line is not connected to empty space.
fn point_is_obstructed(
    p: &lat::Point,
    p_float: &Point3<f32>,
    p_proj: &Point3<f32>,
    voxel_is_empty_fn: &impl Fn(&lat::Point) -> bool,
    config: &CameraCollisionConfig,
) -> bool {
    let p_rej = p_float - p_proj;
    if p_rej.norm_squared() < config.min_obstruction_width.powi(2) {
        return false;
    } else {
        let voxel_p_proj = voxel_containing_point(&p_proj);
        if voxel_is_empty_fn(&voxel_p_proj) {
            // Projection must still be path-connected to empty space.
            let (connected, _) = find_path_through_voxels_with_l1_heuristic(
                &voxel_p_proj,
                p,
                voxel_is_empty_fn,
                config.projection_connection_max_iterations,
            );
            return !connected;
        }
    }

    true
}

fn move_ball_until_collision(
    ball_radius: f32,
    start: &Point3<f32>,
    end: &Point3<f32>,
    voxel_bvt: &VoxelBVT,
    cmp_fn: impl Fn(TOI<f32>, TOI<f32>) -> TOI<f32>,
    predicate_fn: impl Fn(&TOI<f32>) -> bool,
) -> (bool, Point3<f32>) {
    if let Some(impact) = extreme_ball_voxel_impact(
        ball_radius,
        *start,
        *end,
        &voxel_bvt,
        0.0,
        cmp_fn,
        predicate_fn,
    ) {
        // Move ball up until an impact occurs. Make sure not to go in reverse (negative stop_time).
        // Note: this calculation works because `extreme_ball_voxel_impact` ensures the max TOI is
        // 1.0.
        let stop_time = impact.toi;
        debug_assert!(0.0 <= stop_time);
        debug_assert!(stop_time <= 1.0);

        (true, start + stop_time * (end - start))
    } else {
        (false, *end)
    }
}

// ████████╗███████╗███████╗████████╗███████╗
// ╚══██╔══╝██╔════╝██╔════╝╚══██╔══╝██╔════╝
//    ██║   █████╗  ███████╗   ██║   ███████╗
//    ██║   ██╔══╝  ╚════██║   ██║   ╚════██║
//    ██║   ███████╗███████║   ██║   ███████║
//    ╚═╝   ╚══════╝╚══════╝   ╚═╝   ╚══════╝

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CONFIG: CameraCollisionConfig = CameraCollisionConfig {
        ball_radius: 1.0,
        min_obstruction_width: 3.0,
        not_worth_searching_dist: 4.0,
        max_search_iterations: 2000,
        projection_connection_max_iterations: 10,
        camera_lock_threshold: 2.0,
        camera_lock_radius: 0.8,
    };

    #[test]
    fn test_best_unobstructed_range_without_obstructions() {
        let voxel_is_empty_fn = |_p: &lat::Point| true;

        let eye_line =
            Line::from_endpoints(Point3::new(0.0, 0.0, 0.0), Point3::new(10.0, 0.0, 0.0));

        let mut path = Vec::new();
        for i in 0..10 {
            path.push([i, 0, 0].into());
        }

        let ranges = find_unobstructed_ranges(&path, &eye_line, &voxel_is_empty_fn, &TEST_CONFIG);

        assert_eq!(ranges, vec![([0, 10], [0.0, 9.0])]);
    }

    #[test]
    fn test_best_unobstructed_range_with_one_obstruction() {
        // Put a spherical obstruction centered at (0, 0, 0).
        let voxel_is_empty_fn = |p: &lat::Point| {
            let diff = *p - [0, 0, 0].into();

            diff.dot(&diff) > (TEST_CONFIG.min_obstruction_width as i32 + 1).pow(2)
        };

        let eye_line = Line::from_endpoints(Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 0.0, 0.0));

        let start = [-20, 0, 0].into();
        let finish = [100, 0, 0].into();
        let (reached_finish, path) = find_path_through_voxels_with_l1_and_linear_heuristic(
            &start,
            &finish,
            &voxel_is_empty_fn,
            300,
        );
        assert!(reached_finish);

        let ranges = find_unobstructed_ranges(&path, &eye_line, &voxel_is_empty_fn, &TEST_CONFIG);

        assert_eq!(ranges.len(), 2);

        // Second range should start just after the obstacle, which is much closer to the start
        // than the finish, and extend to the very end.
        assert!(ranges[1].0[0] <= 60, "{:?}", ranges[1]);
        assert_eq!(ranges[1].0[1], path.len());
    }
}
