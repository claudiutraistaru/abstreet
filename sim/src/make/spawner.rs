//! Intermediate structures used to instantiate a Scenario. Badly needs simplification:
//! https://github.com/dabreegster/abstreet/issues/258

use serde::{Deserialize, Serialize};

use abstutil::Timer;
use geom::Time;
use map_model::{BuildingID, BusRouteID, BusStopID, Map, PathConstraints, PathRequest, Position};

use crate::{
    CarID, Command, DrivingGoal, PersonID, Scheduler, SidewalkSpot, TripEndpoint, TripLeg,
    TripManager, TripMode, TripPurpose, VehicleType,
};

// TODO Some of these fields are unused now that we separately pass TripEndpoint
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub enum TripSpec {
    /// Can be used to spawn from a border or anywhere for interactive debugging.
    VehicleAppearing {
        start_pos: Position,
        goal: DrivingGoal,
        /// This must be a currently off-map vehicle owned by the person.
        use_vehicle: CarID,
        retry_if_no_room: bool,
    },
    /// Something went wrong spawning the trip.
    SpawningFailure {
        use_vehicle: Option<CarID>,
        error: String,
    },
    UsingParkedCar {
        /// This must be a currently parked vehicle owned by the person.
        car: CarID,
        start_bldg: BuildingID,
        goal: DrivingGoal,
    },
    JustWalking {
        start: SidewalkSpot,
        goal: SidewalkSpot,
    },
    UsingBike {
        bike: CarID,
        start: BuildingID,
        goal: DrivingGoal,
    },
    UsingTransit {
        start: SidewalkSpot,
        goal: SidewalkSpot,
        route: BusRouteID,
        stop1: BusStopID,
        maybe_stop2: Option<BusStopID>,
    },
}

type TripSpawnPlan = (
    PersonID,
    Time,
    TripSpec,
    TripEndpoint,
    TripPurpose,
    bool,
    bool,
);

/// This structure is created temporarily by a Scenario or to interactively spawn agents.
pub struct TripSpawner {
    trips: Vec<TripSpawnPlan>,
}

impl TripSpawner {
    pub fn new() -> TripSpawner {
        TripSpawner { trips: Vec::new() }
    }

    /// Doesn't actually schedule anything yet; you can call this from multiple threads, then feed
    /// all the results to schedule_trips.
    pub fn schedule_trip(
        &self,
        person: PersonID,
        start_time: Time,
        mut spec: TripSpec,
        trip_start: TripEndpoint,
        purpose: TripPurpose,
        cancelled: bool,
        modified: bool,
        map: &Map,
    ) -> TripSpawnPlan {
        // TODO We'll want to repeat this validation when we spawn stuff later for a second leg...
        match &spec {
            TripSpec::VehicleAppearing {
                start_pos,
                goal,
                use_vehicle,
                ..
            } => {
                if start_pos.dist_along() >= map.get_l(start_pos.lane()).length() {
                    panic!("Can't spawn at {}; it isn't that long", start_pos);
                }
                if let DrivingGoal::Border(_, end_lane) = goal {
                    if start_pos.lane() == *end_lane
                        && start_pos.dist_along() == map.get_l(*end_lane).length()
                    {
                        panic!(
                            "Can't start at {}; it's the edge of a border already",
                            start_pos
                        );
                    }
                }

                let constraints = if use_vehicle.1 == VehicleType::Bike {
                    PathConstraints::Bike
                } else {
                    PathConstraints::Car
                };
                if goal.goal_pos(constraints, map).is_none() {
                    spec = TripSpec::SpawningFailure {
                        use_vehicle: Some(use_vehicle.clone()),
                        error: format!("goal_pos to {:?} for a {:?} failed", goal, constraints),
                    };
                }
            }
            TripSpec::SpawningFailure { .. } => {}
            TripSpec::UsingParkedCar { .. } => {}
            TripSpec::JustWalking { start, goal, .. } => {
                if start == goal {
                    panic!(
                        "A trip just walking from {:?} to {:?} doesn't make sense",
                        start, goal
                    );
                }
            }
            TripSpec::UsingBike { start, goal, bike } => {
                // TODO Might not be possible to walk to the same border if there's no sidewalk
                let backup_plan = match goal {
                    DrivingGoal::ParkNear(b) => Some(TripSpec::JustWalking {
                        start: SidewalkSpot::building(*start, map),
                        goal: SidewalkSpot::building(*b, map),
                    }),
                    DrivingGoal::Border(i, _) => {
                        SidewalkSpot::end_at_border(*i, map).map(|goal| TripSpec::JustWalking {
                            start: SidewalkSpot::building(*start, map),
                            goal,
                        })
                    }
                };

                if let Some(start_spot) = SidewalkSpot::bike_rack(*start, map) {
                    if let DrivingGoal::ParkNear(b) = goal {
                        if let Some(goal_spot) = SidewalkSpot::bike_rack(*b, map) {
                            if start_spot.sidewalk_pos.lane() == goal_spot.sidewalk_pos.lane() {
                                info!(
                                    "Bike trip from {} to {} will just walk; it's the same \
                                     sidewalk!",
                                    start, b
                                );
                                spec = backup_plan.unwrap();
                            }
                        } else {
                            info!(
                                "Can't find biking connection for goal {}, walking instead",
                                b
                            );
                            spec = backup_plan.unwrap();
                        }
                    }
                } else if backup_plan.is_some() {
                    info!("Can't start biking from {}. Walking instead", start);
                    spec = backup_plan.unwrap();
                } else {
                    spec = TripSpec::SpawningFailure {
                        use_vehicle: Some(*bike),
                        error: format!(
                            "Can't start biking from {} and can't walk either! Goal is {:?}",
                            start, goal
                        ),
                    };
                }
            }
            TripSpec::UsingTransit { .. } => {}
        };

        (
            person, start_time, spec, trip_start, purpose, cancelled, modified,
        )
    }

    pub fn schedule_trips(&mut self, trips: Vec<TripSpawnPlan>) {
        self.trips.extend(trips);
    }

    pub fn finalize(
        mut self,
        map: &Map,
        trips: &mut TripManager,
        scheduler: &mut Scheduler,
        timer: &mut Timer,
    ) {
        timer.start_iter("spawn trips", self.trips.len());
        for (p, start_time, spec, trip_start, purpose, cancelled, modified) in self.trips.drain(..)
        {
            timer.next();

            // TODO clone() is super weird to do here, but we just need to make the borrow checker
            // happy. All we're doing is grabbing IDs off this.
            let person = trips.get_person(p).unwrap().clone();
            // Just create the trip for each case.
            // TODO Not happy about this clone()
            let trip = match spec.clone() {
                TripSpec::VehicleAppearing {
                    goal, use_vehicle, ..
                } => {
                    let mut legs = vec![TripLeg::Drive(use_vehicle, goal.clone())];
                    if let DrivingGoal::ParkNear(b) = goal {
                        legs.push(TripLeg::Walk(SidewalkSpot::building(b, map)));
                    }
                    trips.new_trip(
                        person.id,
                        start_time,
                        trip_start,
                        if use_vehicle.1 == VehicleType::Bike {
                            TripMode::Bike
                        } else {
                            TripMode::Drive
                        },
                        purpose,
                        modified,
                        legs,
                        map,
                    )
                }
                TripSpec::SpawningFailure { use_vehicle, .. } => {
                    // TODO Need to plumb TripInfo into here
                    todo!()
                    /*let mut legs = vec![TripLeg::Drive(use_vehicle, goal.clone())];
                    if let DrivingGoal::ParkNear(b) = goal {
                        legs.push(TripLeg::Walk(SidewalkSpot::building(b, map)));
                    }
                    trips.new_trip(
                        person.id,
                        start_time,
                        trip_start,
                        if use_vehicle.1 == VehicleType::Bike {
                            TripMode::Bike
                        } else {
                            TripMode::Drive
                        },
                        purpose,
                        modified,
                        legs,
                        map,
                    )*/
                }
                TripSpec::UsingParkedCar { car, goal, .. } => {
                    let mut legs = vec![
                        TripLeg::Walk(SidewalkSpot::deferred_parking_spot()),
                        TripLeg::Drive(car, goal.clone()),
                    ];
                    match goal {
                        DrivingGoal::ParkNear(b) => {
                            legs.push(TripLeg::Walk(SidewalkSpot::building(b, map)));
                        }
                        DrivingGoal::Border(_, _) => {}
                    }
                    trips.new_trip(
                        person.id,
                        start_time,
                        trip_start,
                        TripMode::Drive,
                        purpose,
                        modified,
                        legs,
                        map,
                    )
                }
                TripSpec::JustWalking { goal, .. } => trips.new_trip(
                    person.id,
                    start_time,
                    trip_start,
                    TripMode::Walk,
                    purpose,
                    modified,
                    vec![TripLeg::Walk(goal.clone())],
                    map,
                ),
                TripSpec::UsingBike { bike, start, goal } => {
                    let walk_to = SidewalkSpot::bike_rack(start, map).unwrap();
                    let mut legs = vec![
                        TripLeg::Walk(walk_to.clone()),
                        TripLeg::Drive(bike, goal.clone()),
                    ];
                    match goal {
                        DrivingGoal::ParkNear(b) => {
                            legs.push(TripLeg::Walk(SidewalkSpot::building(b, map)));
                        }
                        DrivingGoal::Border(_, _) => {}
                    };
                    trips.new_trip(
                        person.id,
                        start_time,
                        trip_start,
                        TripMode::Bike,
                        purpose,
                        modified,
                        legs,
                        map,
                    )
                }
                TripSpec::UsingTransit {
                    route,
                    stop1,
                    maybe_stop2,
                    goal,
                    ..
                } => {
                    let walk_to = SidewalkSpot::bus_stop(stop1, map);
                    let legs = if let Some(stop2) = maybe_stop2 {
                        vec![
                            TripLeg::Walk(walk_to.clone()),
                            TripLeg::RideBus(route, Some(stop2)),
                            TripLeg::Walk(goal),
                        ]
                    } else {
                        vec![
                            TripLeg::Walk(walk_to.clone()),
                            TripLeg::RideBus(route, None),
                        ]
                    };
                    trips.new_trip(
                        person.id,
                        start_time,
                        trip_start,
                        TripMode::Transit,
                        purpose,
                        modified,
                        legs,
                        map,
                    )
                }
            };

            if cancelled {
                trips.cancel_unstarted_trip(
                    trip,
                    format!("traffic pattern modifier cancelled this trip"),
                );
            } else {
                scheduler.push(start_time, Command::StartTrip(trip, spec));
            }
        }
    }
}

impl TripSpec {
    pub(crate) fn get_pathfinding_request(&self, map: &Map) -> Option<PathRequest> {
        match self {
            TripSpec::VehicleAppearing {
                start_pos,
                goal,
                use_vehicle,
                ..
            } => {
                let constraints = if use_vehicle.1 == VehicleType::Bike {
                    PathConstraints::Bike
                } else {
                    PathConstraints::Car
                };
                Some(PathRequest {
                    start: *start_pos,
                    end: goal.goal_pos(constraints, map).unwrap(),
                    constraints,
                })
            }
            TripSpec::SpawningFailure { .. } => None,
            // We don't know where the parked car will be
            TripSpec::UsingParkedCar { .. } => None,
            TripSpec::JustWalking { start, goal, .. } => Some(PathRequest {
                start: start.sidewalk_pos,
                end: goal.sidewalk_pos,
                constraints: PathConstraints::Pedestrian,
            }),
            TripSpec::UsingBike { start, .. } => Some(PathRequest {
                start: map.get_b(*start).sidewalk_pos,
                end: SidewalkSpot::bike_rack(*start, map).unwrap().sidewalk_pos,
                constraints: PathConstraints::Pedestrian,
            }),
            TripSpec::UsingTransit { start, stop1, .. } => Some(PathRequest {
                start: start.sidewalk_pos,
                end: SidewalkSpot::bus_stop(*stop1, map).sidewalk_pos,
                constraints: PathConstraints::Pedestrian,
            }),
        }
    }
}
