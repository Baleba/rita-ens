use crate::rita_loop::CLIENT_LOOP_TIMEOUT;
use babel_monitor::{open_babel_stream, parse_routes, Route};
use failure::Error;
use ipnetwork::IpNetwork;
use rita_common::FAST_LOOP_SPEED;
use settings::client::SelectedExit;
use settings::client::{ExitServer, ExitSwitchingCode};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::RwLock;

/// This is the number of metric entries we collect for exit data. Since every tick is 5 sec, and the minimum time we
/// use an exit without swtiching is 15 mins, this values is 15 * 60/5
const METRIC_ENTRIES: usize = (15 * 60) / (FAST_LOOP_SPEED.as_secs() as usize);

lazy_static! {
    /// This lazy static tracks metric values of the exit that we potentially consider switching to during every tick.
    /// To switch, this vector needs to be full of values from a single exit.
    pub static ref METRIC_VALUES: Arc<RwLock<Vec<u16>>> =
        Arc::new(RwLock::new(Vec::with_capacity(METRIC_ENTRIES)));

    pub static ref EXIT_TRACKER: Arc<RwLock<HashMap<IpAddr, ExitTracker>>> = Arc::new(RwLock::new(HashMap::new()));
}

/// This struct contains information about each exit in the cluster. It stores a running total of metric values. This is used to
/// calculate the average metric, and this value wont overflow since we track metric values for no more than 15 mins.
/// Since babel advertises several routes to a given exit, we need to find the route with the best metric and add it to this total. Last_added_metric
/// helps with this by keeping track of what we previosly added to running_total, so that if we come across a better metric to the exit, we
/// subtract this from the total and add the new better value.
#[derive(Default, Debug)]
pub struct ExitTracker {
    last_added_metric: u16,
    running_total: u64,
    ticker_len: u16,
}

impl ExitTracker {
    fn new(last_added_metric: u16, running_total: u64, ticker_len: u16) -> ExitTracker {
        ExitTracker {
            last_added_metric,
            running_total,
            ticker_len,
        }
    }
}

/// Simple struct that keep tracks of the following metrics during every tick:
///
/// 1.) boolean of whether our current exit went down or hasnt been assigned yet,
///
/// 2.) Option<IpAddr> of current exit
///
/// 3.) Babel metric of our current exit
///
/// 4.) Option<IpAddr> of tracking exit
///
/// 5.) Metric of exit we are tracking in lazy static
///
/// 6.) Option<IpAddr> of the best exit during this tick
///
/// 7.) Metric of the best exit during this tick
#[derive(Debug, Clone, Copy)]
struct ExitMetrics {
    is_exit_down: bool,
    cur_exit: Option<IpAddr>,
    cur_exit_babel_met: u16,
    tracking_exit: Option<IpAddr>,
    tracking_met: u16,
    best_exit: Option<IpAddr>,
    best_exit_met: u16,
}

impl ExitMetrics {
    fn new(
        is_exit_down: bool,
        cur_exit: Option<IpAddr>,
        cur_exit_babel_met: u16,
        tracking_exit: Option<IpAddr>,
        tracking_met: u16,
        best_exit: Option<IpAddr>,
        best_exit_met: u16,
    ) -> ExitMetrics {
        ExitMetrics {
            is_exit_down,
            cur_exit,
            cur_exit_babel_met,
            tracking_exit,
            tracking_met,
            best_exit,
            best_exit_met,
        }
    }
}

impl From<ExitMetrics>
    for (
        bool,
        Option<IpAddr>,
        u16,
        Option<IpAddr>,
        u16,
        Option<IpAddr>,
        u16,
    )
{
    fn from(
        e: ExitMetrics,
    ) -> (
        bool,
        Option<IpAddr>,
        u16,
        Option<IpAddr>,
        u16,
        Option<IpAddr>,
        u16,
    ) {
        let ExitMetrics {
            is_exit_down,
            cur_exit,
            cur_exit_babel_met,
            tracking_exit,
            tracking_met,
            best_exit,
            best_exit_met,
        } = e;
        (
            is_exit_down,
            cur_exit,
            cur_exit_babel_met,
            tracking_exit,
            tracking_met,
            best_exit,
            best_exit_met,
        )
    }
}

/// This function helps decides whether we should switch to a better exit or not. It also helps with failover whenever the exit we
/// are currently connected to goes down. The logic works as follows:
///
/// We have a lazy static vector METRIC_VALUES which acts as a timer with 180 spots (1 added every tick ~ 15mins). This is the minimum time we need
/// to wait before we decide whether we want to switch to another exit, given that our current exit is still up. This can also be thought of as a progress bar.
/// To consdier switching our exit, we need to fill up this progress bar with metric values from a single route.
///
/// Every tick, we can be in two situations.
/// Our exit is still up/selected or exit is down/not selected yet. In the situation that the exit we are connected to goes down or is not selected yet,
/// we look through babel routes and choose the best route evaluated by how small its 'metric' value is. This becomes our current exit.
///
/// In the case where our exit is not down, we have the option to switch, but we dont make the switch unless the timer is done/ vector is full. This helps avoid
/// switching due to momentary changes in babel's 'metric'. However during these 15mins, we keep track of metrics so that we can make a decision at the end of 15 mins
/// of whether to switch or not. Tracking of metric logic is as follows:
///
/// There are 3 exits of interest, the exit we are currently connected to, the exit with the lowest babel metric and the exit that we currently tracking in
/// in the lazy static timer, which is a potential candidate to switch to.
/// On subsequent ticks, we check to find what the best exit with lowest metric. To be considered best, its metric has to be lower than our current exit's metric -
/// some degradation constant (This is an approximate measure of how much the route metric increased because of currently being connected to it) and lower than
/// that of the other routes. We subtract this degradation constant to prevent route flapping i.e. constantly switching between two exits because our traffic
/// degrades the metric of the exit we connect to. We add these metric values to the lazy static variables and the best exit becomes our tracking exit. On subsequent
/// ticks this best exit may change, and our tracking exit may differ from our best exit. If during this process, the current exit goes down, we immediately switch
/// to the best exit.
///
/// We can therefore be in different situations given how these exits change.
///
/// 1.) We start tracking the best exit. This means our tracking exit and best exit are same for this tick. If this doesnt change and vector fills up,
/// we can switch the this tracking exit since its been good for long enough. This tracking exit may or may not be the same as our current exit.
///
/// 2.) We are tracking an exit, but the best exit changes to something else before the vector fills up:
///
/// a.) If the new best exit value is > 10% of average tracking values so far, we clear array and start tracking new best exit. We start from (1)
/// b.) If not, there is no point changing the tracking exit, we discard the best exit and continue
///
/// 3.) If the vector eventually gets filled, we know that an exit has been the best for a prolonged period. This exit could either
/// be the current exit we are connected to or a different one. If its a different one we switch to it, else we just clear the vector, and start from (1)
///
/// Look at the enum 'ExitSwitchingCode' to see all state and function 'update_metric_value' to see when these are triggered.
pub fn set_best_exit(
    exits: IpNetwork,
    routes: Vec<Route>,
    rita_client_exit_ser_ref: &mut ExitServer,
) -> Result<IpAddr, Error> {
    if routes.is_empty() {
        bail!("No routes are found")
    }

    // Metric that we advertise which is differnt from babel's advertised metric. Babel_metric - SomeConstant that measures how much our connection degrades the route
    // (ignores the degradation of metric value due to current traffic, unlike the babel Route metric, which smoothens the value)
    let current_adjusted_metric: u16 = rita_client_exit_ser_ref
        .selected_exit
        .selected_id_metric
        .unwrap_or(u16::MAX);
    // Ip of exit we are currently tracking in lazy static, if present
    let tracking_exit = rita_client_exit_ser_ref.selected_exit.tracking_exit;
    // Retrieve current exit ip, if connected
    let current_exit_ip: Option<IpAddr> = rita_client_exit_ser_ref.selected_exit.selected_id;

    let exit_map = &mut *EXIT_TRACKER.write().unwrap();

    // Parse all babel routes and find useful metrics
    let exit_metrics = get_exit_metrics(
        routes,
        exits,
        current_exit_ip,
        tracking_exit,
        current_exit_ip,
        current_adjusted_metric,
        exit_map,
    );

    // When best exit is not set, we are still in initial setup, and no routes are present in the routing table.
    // We simply end the tick and continue the next tick when we have an exit.
    if exit_metrics.best_exit.is_none() {
        bail!("No exit routes found, likely because routing table is empty");
    }

    info!(
        "Exit_Switcher: This tick, we have these metrics: {:?}",
        exit_metrics
    );

    // update lazy static metric and retrieve exit code
    let metric_vec = &mut *METRIC_VALUES.write().unwrap();
    let exit_code = update_metric_value(exit_metrics, metric_vec, exit_map);

    info!(
        "Exit_Switcher: exitCode: {:?}, vector len : {:?}, selected_metric: {:?}, current_exit_babel_met: {:?}, degradation: {:?}",
        exit_code,
        metric_vec.len(),
        current_adjusted_metric,
        exit_metrics.cur_exit_babel_met,
        rita_client_exit_ser_ref.selected_exit.selected_id_degradation
    );

    info!(
        "Exit_Switcher: Our ExitTracker hashmap looks like: {:?}",
        exit_map
    );

    // if exit is down or is not set yet, just return the best exit and reset the lazy static
    if exit_metrics.is_exit_down {
        match exit_metrics.best_exit {
            Some(a) => {
                info!(
                    "Exit_Switcher: setup all initial exit informaion with selected_id_metric = {}",
                    exit_metrics.best_exit_met
                );
                rita_client_exit_ser_ref.selected_exit = SelectedExit {
                    selected_id: exit_metrics.best_exit,
                    selected_id_metric: Some(exit_metrics.best_exit_met),
                    selected_id_degradation: None,
                    tracking_exit: exit_metrics.best_exit,
                };
                metric_vec.clear();
                reset_exit_tracking(exit_map);
                Ok(a)
            }
            None => bail!("Error with finding best exit logic, no exit found"),
        }
    } else {
        //logic to determine wheter we should switch or not.
        set_exit_state(
            rita_client_exit_ser_ref,
            exit_code,
            exit_metrics,
            metric_vec,
        )
    }
}

/// This function looks at the corresponding exit code and makes a decision based on what state we are currently in
fn set_exit_state(
    rita_client_exit_ser_ref: &mut ExitServer,
    exit_code: ExitSwitchingCode,
    exit_metrics: ExitMetrics,
    metric_vec: &mut Vec<u16>,
) -> Result<IpAddr, Error> {
    match exit_code {
        // we get this code when the exit is not setup, meaning it should not reach this else statement in the first place.
        ExitSwitchingCode::InitialExitSetup => panic!("Should not reach this statement"),
        ExitSwitchingCode::ContinueCurrentReset => {
            // We reach this when we continue with the same exit after 15mins of tracking.
            // Degradation is a measure of how much the route metric degrades after connecting to it
            // We set the degradation value = RelU(babel_metric - our_advertised_metric).
            rita_client_exit_ser_ref
                .selected_exit
                .selected_id_degradation = exit_metrics.cur_exit_babel_met.checked_sub(
                rita_client_exit_ser_ref
                    .selected_exit
                    .selected_id_metric
                    .expect("No selected Ip metric where there should be one"),
            );
            Ok(exit_metrics
                .cur_exit
                .expect("Ip value expected, none present"))
        }
        ExitSwitchingCode::ContinueCurrent => {
            // set a degradation values if none, else update the current exit advertised values
            if rita_client_exit_ser_ref
                .selected_exit
                .selected_id_degradation
                .is_none()
            {
                let average_metric = calculate_average(metric_vec.clone());
                // We set degradation value = RelU(average_metric val - our_advertised_metric). Since we know tracking_exit == current_exit,
                // We can use values in the vector.
                rita_client_exit_ser_ref
                    .selected_exit
                    .selected_id_degradation = average_metric.checked_sub(
                    rita_client_exit_ser_ref
                        .selected_exit
                        .selected_id_metric
                        .expect("No selected Ip metric where there should be one"),
                );
            } else {
                // We have already set a degradation value, so we continue using the same value until the clock reset
                let res = exit_metrics.cur_exit_babel_met.checked_sub(
                    rita_client_exit_ser_ref
                        .selected_exit
                        .selected_id_degradation
                        .unwrap(),
                );

                // We should not be setting 'selected_id_metric' as None. If we do, that means degradation > current_metric, meaning an error with logic somewhere
                if res.is_none() {
                    error!("Setting selected_id_metric as none during ExitSwitchingCode::ContinueCurrent. Error with degradation logic");
                } else {
                    rita_client_exit_ser_ref.selected_exit.selected_id_metric = res;
                }
            }
            Ok(exit_metrics
                .cur_exit
                .expect("Ip value expected, none present"))
        }
        ExitSwitchingCode::SwitchExit => {
            // We swtich to the new exit
            rita_client_exit_ser_ref.selected_exit = SelectedExit {
                selected_id: exit_metrics.best_exit,
                selected_id_metric: Some(exit_metrics.best_exit_met),
                selected_id_degradation: None,
                tracking_exit: exit_metrics.best_exit,
            };
            Ok(exit_metrics
                .best_exit
                .expect("Ip value expected, none present"))
        }
        ExitSwitchingCode::ContinueTracking => Ok(exit_metrics
            .cur_exit
            .expect("Ip value expected, none present")),
        ExitSwitchingCode::ResetTracking => {
            // selected id is still the same, we dont change exit, just change what we track
            rita_client_exit_ser_ref.selected_exit.tracking_exit = exit_metrics.best_exit;
            Ok(exit_metrics
                .cur_exit
                .expect("Ip value expected, none present"))
        }
    }
}

/// This function loops through all the routes advertised through babel and searches for 3 particular exits:
///
/// 1.) Current Exit we are connected to, if there is one
///
/// 2.) The Tracking exit that we keep track of in lazy static
///
/// 3.) The best exit with lowest metric, according to babel metrics during this tick
///
/// These values will help us determine the course of action to take, and wheter to switch or not.
/// Once it finds this 3 exits, its returns an ExitMetric struct with the following information:
///
/// 1.) boolean of whether our current exit went down or hasnt been assigned yet,
///
/// 2.) Option<IpAddr> of current exit
///
/// 3.) Babel metric of our current exit
///
/// 4.) Option<IpAddr> of tracking exit
///
/// 5.) Metric of exit we are tracking in lazy static
///
/// 6.) Option<IpAddr> of the best exit during this tick
///
/// 7.) Metric of the best exit during this tick
fn get_exit_metrics(
    routes: Vec<Route>,
    exits: IpNetwork,
    current_exit_ip: Option<IpAddr>,
    tracking_exit: Option<IpAddr>,
    initial_best_exit: Option<IpAddr>,
    initial_best_metric: u16,
    exit_map: &mut HashMap<IpAddr, ExitTracker>,
) -> ExitMetrics {
    let mut best_exit = None;
    let mut best_metric = u16::MAX;
    //By default we say our exit is down. If we find a route to it that is not u16::MAX, we can change this
    let mut current_exit_down = true;

    let mut current_exit_metric = u16::MAX;
    let mut tracking_metric = u16::MAX;

    for route in routes {
        // All babel routes are advertised as /128, so we check if each 'single' ip is part of exit subnet
        let ip = route.prefix.ip();

        if check_ip_in_subnet(ip, exits) {
            //Check to see if our current exit is down
            //current route is down if:
            // 1.) There is not selected_id in rita_exit server(we have not chosen an exit yet)
            // 2.) Our exit ip doesnt exist in babel's routes
            // 3.) Exit's route metric has gone to inf
            if let Some(exit_ip) = current_exit_ip {
                if exit_ip == ip && route.metric != u16::MAX {
                    // Current exit metric is not inf and we have a path to exit, so current exit is up. The time intial_best_metric is
                    // u16::MAX is on rita startup, meaning we have not setup the initial exit yet
                    if initial_best_metric != u16::MAX {
                        current_exit_down = false;
                        current_exit_metric = if current_exit_metric > route.metric {
                            route.metric
                        } else {
                            current_exit_metric
                        };
                    }
                }
            }
            if let Some(tracking_ip) = tracking_exit {
                if tracking_ip == ip && route.metric != u16::MAX {
                    // We are currently tracking an exit, we set its metric. Since babel advertises several routes to an exit, we choose best one
                    tracking_metric = if tracking_metric > route.metric {
                        route.metric
                    } else {
                        tracking_metric
                    };
                }
            }

            info!("Metric for the IP: {} is {}", ip, route.metric);
            // Set details for additional exits in the server
            observe_cluster_metrics(exit_map, ip, route.metric);

            // Every loop iteration, update the best exit
            if route.metric < best_metric {
                best_metric = route.metric;
                best_exit = Some(ip);
            }
        }
    }

    //If current exit is still up, we reset best exit with current exit, using our advertised metric values given that our current exit better
    if !current_exit_down && initial_best_metric < best_metric {
        best_metric = initial_best_metric;
        best_exit = initial_best_exit;
    }

    //We are done adding metrics values to running averages for all exits this tick, so we do cleanup
    set_last_added_to_zero(exit_map);

    ExitMetrics {
        is_exit_down: current_exit_down,
        cur_exit: current_exit_ip,
        cur_exit_babel_met: current_exit_metric,
        tracking_exit,
        tracking_met: tracking_metric,
        best_exit,
        best_exit_met: best_metric,
    }
}

/// This function is called to update the running averages of babel metrics for every exit in the cluster. These average can then
/// be reliabably used to decide which exit to track/switch to. Since babel advertises several routes to exits, we choose the best metric
/// to add to this running average
/// TODO: Add metric tracking to network stat tracker and just query that information here
fn observe_cluster_metrics(exit_map: &mut HashMap<IpAddr, ExitTracker>, ip: IpAddr, met: u16) {
    let met_64 = met as u64;
    if let std::collections::hash_map::Entry::Vacant(e) = exit_map.entry(ip) {
        e.insert(ExitTracker::new(met, met_64, 1));
    } else {
        let exit = exit_map
            .get_mut(&ip)
            .expect("There needs to be an ExitTracker struct for given ip");
        if exit.last_added_metric == 0 {
            exit.running_total += met_64;
            exit.last_added_metric = met;
            exit.ticker_len += 1;
        } else if met < exit.last_added_metric {
            exit.running_total -= exit.last_added_metric as u64;
            exit.running_total += met_64;
            exit.last_added_metric = met;
        }
    }
}

/// After going through a tick of babel metrics for every exits, we may have a residual value for last_added_metric. We set this to 0, so that
/// during the next tick, We dont subtract the previous tick's addition to the running average.
fn set_last_added_to_zero(exit_map: &mut HashMap<IpAddr, ExitTracker>) {
    for (_, v) in exit_map.iter_mut() {
        v.last_added_metric = 0;
    }
}

/// Helper function that checks if the provided ip address is within the subnet specified by the provided IpNetwork
fn check_ip_in_subnet(ip: IpAddr, exits: IpNetwork) -> bool {
    //check that both are either V4 or V6
    if ip.is_ipv4() != exits.is_ipv4() {
        return false;
    }

    exits.contains(ip)
}

/// Updates the current metric value during this tick in the global metric tracker. There can be several situations we can be in since there are
/// 3 exits that we keep track of:
///
/// 1.) The exit we are currently connected to
///
/// 2.) The exit with the best metric value during this tick
///
/// 3.) The exit whose metric values we have been keeping track of in the global vector. The only time (2) and (3) are different is when a new exit
///     becomes the best during a given tick.
///
/// Given that there are several scenarios to be in, this function updates the lazy static with the given metric values, and then return a code indicating
/// what situation we are in so we can act accordingly in the caller funcition. Codes are as follows:
///
/// InitialExitSetup: We have just connected to the first exit and vector is empty
///
/// ContinueCurrentReset: Best exit, current exit and tracking exit are all the same. We continue with the same exit. (Vector is full)
///
/// ContinueCurrent: Same as 1 but vector is not full, not a reset.
///
/// SwitchExit: Current exit is different but tracking and best are same. Vec is full, we switch to best/tracking exit
///
/// ContinueTracking: Current exit is different but tracking == best. Vec is not full, so we dont switch yet, just continue updating
///
/// ResetTracking: tracking and best are diffrent. We reset timer/vector and start tracking new best
fn update_metric_value(
    exit_metrics: ExitMetrics,
    metric_vec: &mut Vec<u16>,
    exit_map: &mut HashMap<IpAddr, ExitTracker>,
) -> ExitSwitchingCode {
    let is_full = metric_vec.len() == metric_vec.capacity();
    let current_exit = exit_metrics.cur_exit;
    let current_metric = exit_metrics.cur_exit_babel_met;
    let best_exit = exit_metrics.best_exit;
    let best_metric = exit_metrics.best_exit_met;
    let tracking_exit = exit_metrics.tracking_exit;
    let tracking_metric = exit_metrics.tracking_met;

    if current_exit.is_none() {
        //setting up exit for first time, vec should be empty
        if !metric_vec.is_empty() {
            panic!("Error with METRIC VALUES update logic");
        }
        return ExitSwitchingCode::InitialExitSetup;
    }

    // if best exit is none, routing table is empty, and thus we still are initial setup
    if best_exit.is_none() {
        panic!("Best exit should not be set to none. If it is it should not be reaching this statement");
    }

    let tracking_exit = match tracking_exit {
        Some(a) => a,
        None => best_exit.unwrap(),
    };

    // We have checked that there are values in best_exit, tracking_exit and current_exit, so we can unwrap safely
    if best_exit.unwrap() == tracking_exit {
        if current_exit.unwrap() == tracking_exit {
            // All three exits are the same
            if is_full {
                metric_vec.clear();
                reset_exit_tracking(exit_map);
                metric_vec.push(best_metric);
                ExitSwitchingCode::ContinueCurrentReset
            } else {
                metric_vec.push(best_metric);
                ExitSwitchingCode::ContinueCurrent
            }
        } else {
            //our current exit is different from the best exit
            if is_full {
                metric_vec.clear();
                reset_exit_tracking(exit_map);
                metric_vec.push(best_metric);
                ExitSwitchingCode::SwitchExit
            } else {
                metric_vec.push(best_metric);
                ExitSwitchingCode::ContinueTracking
            }
        }
    } else {
        // best exit is different from tracking, so we change tracking to be best if we see that the best metric >>> tracking metric
        if worth_switching_tracking_exit(metric_vec, best_exit.unwrap(), exit_map) {
            metric_vec.clear();
            reset_exit_tracking(exit_map);
            metric_vec.push(best_metric);
            ExitSwitchingCode::ResetTracking
        } else {
            // Since we want to continue with current tracking exit, we just make recursive call to best exit == tracking exit
            update_metric_value(
                ExitMetrics::new(
                    exit_metrics.is_exit_down,
                    current_exit,
                    current_metric,
                    Some(tracking_exit),
                    tracking_metric,
                    Some(tracking_exit),
                    tracking_metric,
                ),
                metric_vec,
                exit_map,
            )
        }
    }
}

fn reset_exit_tracking(exit_map: &mut HashMap<IpAddr, ExitTracker>) {
    for (_, v) in exit_map.iter_mut() {
        v.last_added_metric = 0;
        v.running_total = 0;
        v.ticker_len = 0;
    }
}

/// It is worth tracking a new better exit only if its values is more than 10% better than our current tracking exit values, else there is no point
/// throwing away all our progress for our current tracking exit. This helps solve the following edge case:
///
/// We are connected to exit A, exit B and C are consistently better than A, but they flucuate between being the best exit every tick. Instead of
/// reseting our tracking exit every tick, we continue with one exit, either B or C, unless one exit becomes significantly better than the other. This way
/// we dont get stuck at exit A when there are better exits available.
fn worth_switching_tracking_exit(
    metric_vec: &mut Vec<u16>,
    best_ip: IpAddr,
    exit_map: &mut HashMap<IpAddr, ExitTracker>,
) -> bool {
    if metric_vec.is_empty() {
        return false;
    }
    let avg_tracking_metric = calculate_average(metric_vec.clone());

    let exit_tracker = exit_map
        .get(&best_ip)
        .expect("There should be an ExitTracker entry here");
    if exit_tracker.ticker_len == 0 {
        return false;
    }
    let avg_best_metric = (exit_tracker.running_total / exit_tracker.ticker_len as u64) as u16;

    if avg_tracking_metric < avg_best_metric || avg_best_metric == 0 {
        false
    } else {
        (((avg_tracking_metric - avg_best_metric) as f64) / (avg_tracking_metric as f64)) > 0.1
    }
}

/// Given a vector of u16, calculates the average. Panics if given a vector with no entries
fn calculate_average(vals: Vec<u16>) -> u16 {
    if vals.is_empty() {
        panic!("received list of values with no elements");
    }
    let mut sum: u64 = 0;
    for entry in vals.iter() {
        sum += *entry as u64;
    }

    (sum / vals.len() as u64) as u16
}

/// Simple helper function that opens a babel stream to get all routes related to us. We can use these routes to
/// check which ips are exits and thereby register or setup exits
pub fn get_babel_routes(babel_port: u16) -> Result<Vec<Route>, Error> {
    let mut stream = match open_babel_stream(babel_port, CLIENT_LOOP_TIMEOUT) {
        Ok(a) => a,
        Err(_) => {
            bail!("open babel stream error in exit manager tick");
        }
    };
    let routes = match parse_routes(&mut stream) {
        Ok(a) => a,
        Err(_) => {
            bail!("Parse routes error in exit manager tick");
        }
    };

    Ok(routes)
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_calculate_average() {
        let vec = vec![10];

        assert_eq!(calculate_average(vec), 10);

        let vec = vec![10, 10, 12, 16, 20];

        // we map 13.6 -> u16
        assert_eq!(calculate_average(vec), 13);
    }

    #[test]
    fn test_worth_switching_tracking() {
        let mut vec: Vec<u16> = vec![100];
        let mut exit_map: HashMap<IpAddr, ExitTracker> = HashMap::new();
        let ip = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));

        exit_map.insert(ip, ExitTracker::new(110, 110, 1));
        assert!(!worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));

        exit_map.insert(ip, ExitTracker::new(111, 111, 1));
        assert!(!worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));

        exit_map.insert(ip, ExitTracker::new(90, 90, 1));
        assert!(!worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));

        exit_map.insert(ip, ExitTracker::new(89, 89, 1));
        assert!(worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));

        //avg is 13.6 -> to u16 -> 13
        let mut vec = vec![10, 10, 12, 16, 20];

        exit_map.insert(ip, ExitTracker::new(12, 12 * 5, 5));
        assert!(!worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));

        exit_map.insert(ip, ExitTracker::new(11, 11 * 5, 5));
        assert!(worth_switching_tracking_exit(&mut vec, ip, &mut exit_map));
    }

    #[test]
    fn test_update_metric_values() {
        let mut vec: Vec<u16> = Vec::with_capacity(10);
        let mut exit_map: HashMap<IpAddr, ExitTracker> = HashMap::new();
        let ip = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        exit_map.insert(ip, ExitTracker::new(110, 110, 1));

        // we use ipv6 addrs, but this should also work with ipv4

        // Testing for initial error code, when we just start tracking. No current or tracking
        let best_exit = Some(IpAddr::V4(Ipv4Addr::new(1, 12, 12, 12)));
        let mut tracking_exit: Option<IpAddr> = None;
        let mut current_exit = None;

        assert_eq!(
            ExitSwitchingCode::InitialExitSetup,
            update_metric_value(
                ExitMetrics::new(
                    true,
                    current_exit,
                    u16::MAX,
                    tracking_exit,
                    u16::MAX,
                    best_exit,
                    400
                ),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 0);

        //during second tick, current exit should be set. Ideally its the same as best_exit. Tracking exit is the same as best exit
        tracking_exit = best_exit;
        current_exit = best_exit;

        assert_eq!(
            ExitSwitchingCode::ContinueCurrent,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 450, tracking_exit, 450, best_exit, 450),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 1);
        assert_eq!(vec[0], 450);

        // Testing when the vec gets full. All three exits are same.
        let mut append: Vec<u16> = vec![400, 420, 430, 420, 400, 450, 430, 410, 400];
        vec.append(&mut append);
        assert_eq!(vec.len(), 10);

        assert_eq!(
            ExitSwitchingCode::ContinueCurrentReset,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 415, tracking_exit, 415, best_exit, 415),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.capacity(), 10);
        assert_eq!(vec.len(), 1);
        assert_eq!(vec[0], 415);

        // The best exit changes, but metric is not good enough to cause a change. If if the next sample is insanely large,
        // we look at the vector to make a descision. Best exit is different from tracking == current.
        current_exit = Some(IpAddr::V4(Ipv4Addr::new(1, 120, 120, 120)));
        tracking_exit = current_exit;

        exit_map.insert(best_exit.unwrap(), ExitTracker::new(0, 91030, 1));

        assert_eq!(
            ExitSwitchingCode::ContinueCurrent,
            update_metric_value(
                ExitMetrics::new(
                    false,
                    current_exit,
                    5000,
                    tracking_exit,
                    5000,
                    best_exit,
                    413
                ),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 2);
        assert_eq!(vec[1], 5000);

        // we start tracking something else other than current. Tracking == best different from current
        tracking_exit = best_exit;

        assert_eq!(
            ExitSwitchingCode::ContinueTracking,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 500, tracking_exit, 410, best_exit, 410),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 3);
        assert_eq!(vec[2], 410);

        // tracking different than current, and we fill up. We then reset the exit. Tracking == best different from current
        append = vec![410, 410, 400, 400, 430, 430, 410];
        vec.append(&mut append);
        assert_eq!(vec.len(), 10);

        assert_eq!(
            ExitSwitchingCode::SwitchExit,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 500, tracking_exit, 410, best_exit, 410),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.capacity(), 10);

        vec.clear();
        // when best exit changes last minute, but isnt good enough, we still get SwitchExit. All three exit are different.
        append = vec![410, 410, 400, 400, 430, 430, 410, 410, 430, 400];
        vec.append(&mut append);
        assert_eq!(vec.len(), 10);

        tracking_exit = Some(IpAddr::V4(Ipv4Addr::new(1, 200, 200, 200)));

        assert_eq!(
            ExitSwitchingCode::SwitchExit,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 500, tracking_exit, 450, best_exit, 440),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.capacity(), 10);

        vec.clear();
        // However when the exit is better, we switch, even when we have a full bar. All 3 exits are different
        append = vec![410, 410, 400, 400, 430, 430, 410, 410, 430, 400];
        vec.append(&mut append);
        assert_eq!(vec.len(), 10);
        exit_map.insert(best_exit.unwrap(), ExitTracker::new(0, 10, 1));

        assert_eq!(
            ExitSwitchingCode::ResetTracking,
            update_metric_value(
                ExitMetrics::new(false, current_exit, 500, tracking_exit, 450, best_exit, 200),
                &mut vec,
                &mut exit_map
            )
        );
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.capacity(), 10);
    }

    #[test]
    fn test_ip_in_subnet() {
        let subnet = IpNetwork::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 20).unwrap();
        let ip1 = IpAddr::V4(Ipv4Addr::new(1, 2, 1, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(1, 1, 7, 127));
        let ip3 = IpAddr::V4(Ipv4Addr::new(1, 1, 20, 127));
        let ip4 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 12));

        assert!(!check_ip_in_subnet(ip1, subnet));
        assert!(check_ip_in_subnet(ip2, subnet));
        assert!(!check_ip_in_subnet(ip3, subnet));
        assert!(check_ip_in_subnet(ip4, subnet));
    }

    #[test]
    fn test_get_exit_metrics() {
        let subnet = IpNetwork::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 20).unwrap();
        let ip1 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 2));
        let ip3 = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 3));
        let random_ip = IpAddr::V4(Ipv4Addr::new(2, 1, 1, 5));
        //set up some fake routes, only think that amtters is prefix and metric
        let exit1 = Route {
            id: "a".to_string(),
            iface: "a".to_string(),
            xroute: false,
            installed: false,
            neigh_ip: random_ip,
            prefix: IpNetwork::new(ip1, 32).unwrap(),
            metric: 400,
            refmetric: 400,
            full_path_rtt: 10.0,
            price: 10,
            fee: 10,
        };

        let exit2 = Route {
            id: "a".to_string(),
            iface: "a".to_string(),
            xroute: false,
            installed: false,
            neigh_ip: random_ip,
            prefix: IpNetwork::new(ip2, 32).unwrap(),
            metric: 500,
            refmetric: 400,
            full_path_rtt: 10.0,
            price: 10,
            fee: 10,
        };

        let exit3 = Route {
            id: "a".to_string(),
            iface: "a".to_string(),
            xroute: false,
            installed: false,
            neigh_ip: random_ip,
            prefix: IpNetwork::new(ip3, 32).unwrap(),
            metric: 200,
            refmetric: 400,
            full_path_rtt: 10.0,
            price: 10,
            fee: 10,
        };

        let not_exit = Route {
            id: "a".to_string(),
            iface: "a".to_string(),
            xroute: false,
            installed: false,
            neigh_ip: random_ip,
            prefix: IpNetwork::new(random_ip, 32).unwrap(),
            metric: 100,
            refmetric: 400,
            full_path_rtt: 10.0,
            price: 10,
            fee: 10,
        };

        let routes = vec![exit1, exit2, exit3, not_exit];
        let mut exit_map: HashMap<IpAddr, ExitTracker> = HashMap::new();

        // Nothing is setup yet
        let (exit_down, _, c_e_met, _, t_e_m, b_exit, b_e_m) = get_exit_metrics(
            routes.clone(),
            subnet,
            None,
            None,
            None,
            u16::MAX,
            &mut exit_map,
        )
        .into();
        assert!(exit_down);
        assert_eq!(c_e_met, u16::MAX);
        assert_eq!(t_e_m, u16::MAX);
        assert_eq!(b_exit.unwrap(), ip3);
        assert_eq!(b_e_m, 200);

        // Only current exit is setup, not tracking yet
        let (exit_down, _, c_e_met, _, t_e_m, b_exit, b_e_m) = get_exit_metrics(
            routes.clone(),
            subnet,
            Some(ip1),
            None,
            Some(ip1),
            400,
            &mut exit_map,
        )
        .into();
        assert!(!exit_down);
        assert_eq!(c_e_met, 400);
        assert_eq!(t_e_m, u16::MAX);
        assert_eq!(b_exit.unwrap(), ip3);
        assert_eq!(b_e_m, 200);

        // current and tracking at setup and different from each other and best exit
        let (exit_down, _, c_e_met, _, t_e_m, b_exit, b_e_m) = get_exit_metrics(
            routes.clone(),
            subnet,
            Some(ip1),
            Some(ip2),
            Some(ip1),
            500,
            &mut exit_map,
        )
        .into();
        assert!(!exit_down);
        assert_eq!(c_e_met, 400);
        assert_eq!(t_e_m, 500);
        assert_eq!(b_exit.unwrap(), ip3);
        assert_eq!(b_e_m, 200);

        // Current and tracking are same but different from best exit
        let (exit_down, _, c_e_met, _, t_e_m, b_exit, b_e_m) = get_exit_metrics(
            routes.clone(),
            subnet,
            Some(ip2),
            Some(ip2),
            Some(ip2),
            500,
            &mut exit_map,
        )
        .into();
        assert!(!exit_down);
        assert_eq!(c_e_met, 500);
        assert_eq!(t_e_m, 500);
        assert_eq!(b_exit.unwrap(), ip3);
        assert_eq!(b_e_m, 200);

        // All three exits are the same
        let (exit_down, _, c_e_met, _, t_e_m, b_exit, b_e_m) = get_exit_metrics(
            routes,
            subnet,
            Some(ip3),
            Some(ip3),
            Some(ip3),
            200,
            &mut exit_map,
        )
        .into();
        assert!(!exit_down);
        assert_eq!(c_e_met, 200);
        assert_eq!(t_e_m, 200);
        assert_eq!(b_exit.unwrap(), ip3);
        assert_eq!(b_e_m, 200);
    }

    #[test]
    fn test_config_update() {
        use settings::client::RitaClientSettings;

        let path = "./src/exit_manager/config_in_use.toml".to_string();
        let settings = RitaClientSettings::new(&path).unwrap();

        assert_eq!(
            settings.exit_client.current_exit,
            settings.old_exit_client.current_exit
        );
        assert_eq!(
            settings.exit_client.wg_listen_port,
            settings.old_exit_client.wg_listen_port
        );
        assert_eq!(
            settings.exit_client.contact_info,
            settings.old_exit_client.contact_info
        );
        assert_eq!(
            settings.exit_client.lan_nics,
            settings.old_exit_client.lan_nics
        );
        assert_eq!(
            settings.exit_client.low_balance_notification,
            settings.old_exit_client.low_balance_notification
        );

        println!("Old Settings: {:?}", settings.old_exit_client.exits);
        println!("\n\n\n\nNew Settings: {:?}", settings.exit_client.exits);
    }
}
