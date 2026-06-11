use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    thread::{Builder, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, RecvError};
use dashmap::DashMap;
use itertools::Itertools;
use jito_protos::shredstream::{Entry as PbEntry, TraceShred};
use log::{debug, error, info, warn};
use prost::Message;
use solana_client::client_error::reqwest;
use solana_ledger::shred::ReedSolomonCache;
use solana_metrics::{datapoint_info, datapoint_warn};
use solana_net_utils::SocketConfig;
use solana_perf::{
    deduper::Deduper,
    packet::{PacketBatch, PacketBatchRecycler},
    recycler::Recycler,
};
use solana_sdk::clock::Slot;
use solana_streamer::{
    sendmmsg::{batch_send, SendPktsError},
    streamer::{self, StreamerReceiveStats},
};
use tokio::sync::broadcast::Sender;

use crate::{
    deshred,
    deshred::{ComparableShred, ShredsStateTracker},
    resolve_hostname_port, ShredstreamProxyError,
};

// values copied from https://github.com/solana-labs/solana/blob/33bde55bbdde13003acf45bb6afe6db4ab599ae4/core/src/sigverify_shreds.rs#L20
pub const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
pub const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
pub const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

/// Upper bounds (µs) of the dup-lag histogram buckets; observations above the
/// last bound land in the implicit +Inf bucket (`count`).
pub const DUP_LAG_BUCKETS_US: [u64; 13] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
];
/// Prometheus `le` label values (seconds) matching `DUP_LAG_BUCKETS_US`.
const DUP_LAG_LE_SECONDS: [&str; 13] = [
    "0.00005", "0.0001", "0.00025", "0.0005", "0.001", "0.0025", "0.005", "0.01", "0.025", "0.05",
    "0.1", "0.25", "0.5",
];

struct FirstSeen {
    when: Instant,
    addr: IpAddr,
    port: u16,
}

#[derive(Default)]
pub struct DupLagHistogram {
    /// One counter per `DUP_LAG_BUCKETS_US` band (made cumulative at report time).
    buckets: [AtomicU64; 13],
    /// Observations above the last bound.
    overflow: AtomicU64,
    pub count: AtomicU64,
    pub sum_micros: AtomicU64,
}

/// Measures the lag between a shred's first arrival and later duplicate
/// arrivals of the same bytes, attributed by (winner, loser) source pair.
/// Used to quantify how much earlier one shred provider delivers than another.
pub struct DupLagTracker {
    /// Two time-rotated shards: lookups check both, inserts go to `cur`.
    /// Rotation drops the older shard, bounding retention to [ttl, 2*ttl).
    shards: [DashMap<u64, FirstSeen>; 2],
    cur: AtomicUsize,
    /// Next rotation, as µs since `anchor` (CAS claims the rotation).
    rotate_deadline_us: AtomicU64,
    anchor: Instant,
    ttl_us: u64,
    /// Sample a packet when `hash & sample_mask == 0`. Hash-based (not random)
    /// so the winner and loser arrivals of the same shred are both sampled.
    sample_mask: u64,
    hasher: ahash::RandomState,
    /// Lifetime-cumulative histograms keyed by
    /// (winner addr, winner port, loser addr, loser port).
    pub hist: DashMap<(IpAddr, u16, IpAddr, u16), DupLagHistogram>,
}

impl DupLagTracker {
    pub fn new(ttl: Duration, sample_rate: u32) -> Self {
        assert!(
            sample_rate.is_power_of_two(),
            "dup_lag_sample_rate must be a power of two"
        );
        let ttl_us = ttl.as_micros().max(1) as u64;
        Self {
            shards: [DashMap::new(), DashMap::new()],
            cur: AtomicUsize::new(0),
            rotate_deadline_us: AtomicU64::new(ttl_us),
            anchor: Instant::now(),
            ttl_us,
            sample_mask: sample_rate as u64 - 1,
            hasher: ahash::RandomState::new(),
            hist: DashMap::new(),
        }
    }

    #[cfg(test)]
    fn new_seeded(ttl: Duration, sample_rate: u32, seed: u64) -> Self {
        let mut tracker = Self::new(ttl, sample_rate);
        tracker.hasher = ahash::RandomState::with_seeds(seed, seed ^ 1, seed ^ 2, seed ^ 3);
        tracker
    }

    fn maybe_rotate(&self, now: Instant) {
        let now_us = now.duration_since(self.anchor).as_micros() as u64;
        let deadline = self.rotate_deadline_us.load(Ordering::Relaxed);
        if now_us < deadline {
            return;
        }
        if self
            .rotate_deadline_us
            .compare_exchange(
                deadline,
                now_us + self.ttl_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            let cur = self.cur.load(Ordering::Relaxed);
            let prev = cur ^ 1;
            self.shards[prev].clear();
            // After an idle gap of a full extra period, `cur` is also entirely
            // older than the retention window — drop it too so stale entries
            // can't match late re-arrivals and inflate lag sums.
            if now_us >= deadline.saturating_add(self.ttl_us) {
                self.shards[cur].clear();
            }
            // the cleared shard becomes the new insert target; the old `cur` ages out next rotation
            self.cur.store(prev, Ordering::Relaxed);
        }
    }

    fn record_lag(&self, first: &FirstSeen, now: Instant, addr: IpAddr, port: u16) {
        let lag_us = now.duration_since(first.when).as_micros() as u64;
        let hist = self
            .hist
            .entry((first.addr, first.port, addr, port))
            .or_default();
        match DUP_LAG_BUCKETS_US.iter().position(|bound| lag_us <= *bound) {
            Some(i) => hist.buckets[i].fetch_add(1, Ordering::Relaxed),
            None => hist.overflow.fetch_add(1, Ordering::Relaxed),
        };
        hist.count.fetch_add(1, Ordering::Relaxed);
        hist.sum_micros.fetch_add(lag_us, Ordering::Relaxed);
    }

    /// Record one packet arrival. First sighting of these bytes stores
    /// (now, source); a later sighting within the retention window records the
    /// lag into the (winner, loser) histogram. Must run BEFORE dedup marks
    /// discards: `Packet::data()` hides payloads of discarded packets, and the
    /// duplicates are exactly what we need to observe.
    pub fn observe(&self, data: &[u8], now: Instant, addr: IpAddr, port: u16) {
        // Rotate on every arrival (one relaxed load when not due) so the
        // retention window holds regardless of the sampling rate.
        self.maybe_rotate(now);
        let hash = self.hasher.hash_one(data);
        if hash & self.sample_mask != 0 {
            return;
        }
        let cur = self.cur.load(Ordering::Relaxed);
        // The older shard only ever loses entries (inserts target `cur`), so a
        // plain read is race-free here.
        if let Some(first) = self.shards[cur ^ 1].get(&hash) {
            self.record_lag(&first, now, addr, port);
            return;
        }
        // Atomic check-or-insert on the current shard: two threads observing
        // the same shred concurrently must not both treat it as first-seen
        // (a double insert would drop the race record and could crown the
        // later arrival as winner).
        match self.shards[cur].entry(hash) {
            dashmap::mapref::entry::Entry::Occupied(first) => {
                self.record_lag(first.get(), now, addr, port);
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(FirstSeen { when: now, addr, port });
            }
        }
    }
}

/// Bind to ports and start forwarding shreds
#[allow(clippy::too_many_arguments)]
pub fn start_forwarder_threads(
    unioned_dest_sockets: Arc<ArcSwap<Vec<SocketAddr>>>, /* sockets shared between endpoint discovery thread and forwarders */
    src_addr: IpAddr,
    src_port: u16,
    multicast_subscribe_port: u16,
    multicast_device: String,
    maybe_multicast_socket: Option<Vec<UdpSocket>>,
    num_threads: Option<usize>,
    deduper: Arc<RwLock<Deduper<2, [u8]>>>,
    should_reconstruct_shreds: bool,
    entry_sender: Arc<Sender<PbEntry>>,
    debug_trace_shred: bool,
    use_discovery_service: bool,
    forward_stats: Arc<StreamerReceiveStats>,
    metrics: Arc<ShredMetrics>,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let num_threads = num_threads
        .unwrap_or_else(|| usize::from(std::thread::available_parallelism().unwrap()).min(4));

    let recycler: PacketBatchRecycler = Recycler::warmed(100, 1024);

    // multi_bind_in_range returns (port, Vec<UdpSocket>)
    let (_port, sockets) = solana_net_utils::multi_bind_in_range_with_config(
        src_addr,
        (src_port, src_port + 1),
        SocketConfig::default().reuseport(true),
        num_threads,
    )
    .unwrap_or_else(|_| {
        panic!("Failed to bind listener sockets. Check that port {src_port} is not in use.")
    });

    let (reconstruct_tx, reconstruct_rx) = crossbeam_channel::bounded(1_024);
    let mut thread_hdls = Vec::with_capacity(num_threads + 1);

    if should_reconstruct_shreds {
        let metrics = metrics.clone();
        let exit = exit.clone();
        // receives shreds from recv_from_channel_and_send_multiple_dest and calls deshred::reconstruct_shreds
        let hdl = std::thread::Builder::new()
            .name("shred_reconstructor".to_string())
            .spawn(move || {
                let mut all_shreds = ahash::HashMap::<
                    Slot,
                    (
                        ahash::HashMap<u32, HashSet<ComparableShred>>,
                        ShredsStateTracker,
                    ),
                >::default();
                let mut slot_fec_indexes_to_iterate = Vec::<(Slot, u32)>::new();
                let mut deshredded_entries =
                    Vec::<(Slot, Vec<solana_entry::entry::Entry>, Vec<u8>)>::new();
                let mut highest_slot_seen: Slot = 0;
                let rs_cache = ReedSolomonCache::default();

                while !exit.load(Ordering::Relaxed) {
                    match reconstruct_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(pkt_batch) => {
                            deshred::reconstruct_shreds(
                                pkt_batch,
                                &mut all_shreds,
                                &mut slot_fec_indexes_to_iterate,
                                &mut deshredded_entries,
                                &mut highest_slot_seen,
                                &rs_cache,
                                &metrics,
                            );

                            deshredded_entries.drain(..).for_each(
                                |(slot, _entries, entries_bytes)| {
                                    let _ = entry_sender.send(PbEntry {
                                        slot,
                                        entries: entries_bytes,
                                    });
                                },
                            );
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {} // do nothing
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .unwrap();
        thread_hdls.push(hdl);
    };

    sockets
        .into_iter()
        .chain(maybe_multicast_socket.into_iter().flatten())
        .enumerate()
        .flat_map(|(thread_id, incoming_shred_socket)| {
            let listen_port = incoming_shred_socket
                .local_addr()
                .expect("bound socket has local addr")
                .port();
            let device_label = if listen_port == multicast_subscribe_port {
                multicast_device.as_str()
            } else {
                "unicast"
            };
            info!("Forwarder socket {thread_id}: listen_port={listen_port} device={device_label}");

            let (packet_sender, packet_receiver) = crossbeam_channel::unbounded();
            let listen_thread = streamer::receiver(
                format!("ssListen{thread_id}"),
                Arc::new(incoming_shred_socket),
                exit.clone(),
                packet_sender,
                recycler.clone(),
                forward_stats.clone(),
                Duration::default(),
                false,
                None,
                false,
            );

            let deduper = deduper.clone();
            let unioned_dest_sockets = unioned_dest_sockets.clone();
            let metrics = metrics.clone();
            let shutdown_receiver = shutdown_receiver.clone();
            let reconstruct_tx = reconstruct_tx.clone();
            let exit = exit.clone();

            let send_thread = Builder::new()
                .name(format!("ssPxyTx_{thread_id}"))
                .spawn(move || {
                    let send_socket =
                        UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
                            .expect("to bind to udp port for forwarding");
                    let mut local_dest_sockets = unioned_dest_sockets.load();

                    let refresh_subscribers_tick = if use_discovery_service {
                        crossbeam_channel::tick(Duration::from_secs(30))
                    } else {
                        crossbeam_channel::tick(Duration::MAX)
                    };

                    while !exit.load(Ordering::Relaxed) {
                        crossbeam_channel::select! {
                            // forward packets
                            recv(packet_receiver) -> maybe_packet_batch => {
                                let res = recv_from_channel_and_send_multiple_dest(
                                    maybe_packet_batch,
                                    listen_port,
                                    &deduper,
                                    &send_socket,
                                    &local_dest_sockets,
                                    should_reconstruct_shreds,
                                    &reconstruct_tx,
                                    debug_trace_shred,
                                    &metrics,
                                );

                                // If the channel is closed or error, break out
                                if res.is_err() {
                                    break;
                                }
                            }

                            // refresh thread-local subscribers
                            recv(refresh_subscribers_tick) -> _ => {
                                local_dest_sockets = unioned_dest_sockets.load();
                            }

                            // handle shutdown (avoid using sleep since it can hang)
                            recv(shutdown_receiver) -> _ => {
                                break;
                            }
                        }
                    }
                    info!("Exiting forwarder thread {thread_id}.");
                })
                .unwrap();

            vec![listen_thread, send_thread]
        })
        .collect::<Vec<JoinHandle<()>>>()
}

/// Broadcasts the same packet to multiple recipients, parses it into a Shred if possible,
/// and stores that shred in `all_shreds`.
#[allow(clippy::too_many_arguments)]
fn recv_from_channel_and_send_multiple_dest(
    maybe_packet_batch: Result<PacketBatch, RecvError>,
    listen_port: u16,
    deduper: &RwLock<Deduper<2, [u8]>>,
    send_socket: &UdpSocket,
    local_dest_sockets: &[SocketAddr],
    should_reconstruct_shreds: bool,
    reconstruct_tx: &crossbeam_channel::Sender<PacketBatch>,
    debug_trace_shred: bool,
    metrics: &ShredMetrics,
) -> Result<(), ShredstreamProxyError> {
    let packet_batch = maybe_packet_batch.map_err(ShredstreamProxyError::RecvError)?;
    let trace_shred_received_time = SystemTime::now();
    metrics
        .received
        .fetch_add(packet_batch.len() as u64, Ordering::Relaxed);
    debug!(
        "Got batch of {} packets, total size in bytes: {}",
        packet_batch.len(),
        packet_batch.iter().map(|x| x.meta().size).sum::<usize>()
    );

    if should_reconstruct_shreds {
        let _ = reconstruct_tx.try_send(packet_batch.clone());
    }

    let mut packet_batch_vec = vec![packet_batch];

    // Observe BEFORE dedup: Packet::data() returns None once a packet is marked
    // discard, and duplicate arrivals are exactly what dup-lag must see.
    if let Some(dup_lag) = metrics.dup_lag.as_ref() {
        let now = Instant::now();
        packet_batch_vec[0].iter().for_each(|packet| {
            if let Some(data) = packet.data(..) {
                dup_lag.observe(data, now, packet.meta().addr, listen_port);
            }
        });
    }

    let num_deduped = solana_perf::deduper::dedup_packets_and_count_discards(
        &deduper.read().unwrap(),
        &mut packet_batch_vec,
    );
    // Store stats for each Packet, keyed by (source IP, local listen port).
    // listen_port is constant for this thread (one socket per send_thread), so packets
    // arriving on the multicast vs unicast socket get attributed separately even when
    // they share a source IP.
    packet_batch_vec.iter().for_each(|batch| {
        batch.iter().for_each(|packet| {
            metrics
                .packets_received
                .entry((packet.meta().addr, listen_port))
                .and_modify(|(discarded, not_discarded)| {
                    *discarded += packet.meta().discard() as u64;
                    *not_discarded += (!packet.meta().discard()) as u64;
                })
                .or_insert_with(|| {
                    (
                        packet.meta().discard() as u64,
                        (!packet.meta().discard()) as u64,
                    )
                });
        });
    });

    // send out to RPCs
    local_dest_sockets.iter().for_each(|outgoing_socketaddr| {
        let packets_with_dest = packet_batch_vec[0]
            .iter()
            .filter_map(|pkt| {
                let data = pkt.data(..)?;
                let addr = outgoing_socketaddr;
                Some((data, addr))
            })
            .collect::<Vec<(&[u8], &SocketAddr)>>();

        match batch_send(send_socket, &packets_with_dest) {
            Ok(_) => {
                metrics
                    .success_forward
                    .fetch_add(packets_with_dest.len() as u64, Ordering::Relaxed);
                metrics.duplicate.fetch_add(num_deduped, Ordering::Relaxed);
            }
            Err(SendPktsError::IoError(err, num_failed)) => {
                metrics
                    .fail_forward
                    .fetch_add(packets_with_dest.len() as u64, Ordering::Relaxed);
                metrics
                    .duplicate
                    .fetch_add(num_failed as u64, Ordering::Relaxed);
                error!(
                    "Failed to send batch of size {} to {outgoing_socketaddr:?}. \
                     {num_failed} packets failed. Error: {err}",
                    packets_with_dest.len()
                );
            }
        }
    });

    // Count TraceShred shreds
    if debug_trace_shred {
        packet_batch_vec[0]
            .iter()
            .filter_map(|p| TraceShred::decode(p.data(..)?).ok())
            .filter(|t| t.created_at.is_some())
            .for_each(|trace_shred| {
                let elapsed = trace_shred_received_time
                    .duration_since(SystemTime::try_from(trace_shred.created_at.unwrap()).unwrap())
                    .unwrap_or_default();

                datapoint_info!(
                    "shredstream_proxy-trace_shred_latency",
                    "trace_region" => trace_shred.region,
                    ("trace_seq_num", trace_shred.seq_num as i64, i64),
                    ("elapsed_micros", elapsed.as_micros(), i64),
                );
            });
    }

    Ok(())
}

/// Starts a thread that updates our destinations used by the forwarder threads
pub fn start_destination_refresh_thread(
    endpoint_discovery_url: String,
    discovered_endpoints_port: u16,
    static_dest_sockets: Vec<(SocketAddr, String)>,
    unioned_dest_sockets: Arc<ArcSwap<Vec<SocketAddr>>>,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    Builder::new().name("ssPxyDstRefresh".to_string()).spawn(move || {
        let fetch_socket_tick = crossbeam_channel::tick(Duration::from_secs(30));
        let metrics_tick = crossbeam_channel::tick(Duration::from_secs(30));
        let mut socket_count = static_dest_sockets.len();
        while !exit.load(Ordering::Relaxed) {
            crossbeam_channel::select! {
                    recv(fetch_socket_tick) -> _ => {
                        let fetched = fetch_unioned_destinations(
                            &endpoint_discovery_url,
                            discovered_endpoints_port,
                            &static_dest_sockets,
                        );
                        let new_sockets = match fetched {
                            Ok(s) => {
                                info!("Sending shreds to {} destinations: {s:?}", s.len());
                                s
                            }
                            Err(e) => {
                                warn!("Failed to fetch from discovery service, retrying. Error: {e}");
                                datapoint_warn!("shredstream_proxy-destination_refresh_error",
                                                ("prev_unioned_dest_count", socket_count, i64),
                                                ("errors", 1, i64),
                                                ("error_str", e.to_string(), String),
                                );
                                continue;
                            }
                        };
                        socket_count = new_sockets.len();
                        unioned_dest_sockets.store(Arc::new(new_sockets));
                    }
                    recv(metrics_tick) -> _ => {
                        datapoint_info!("shredstream_proxy-destination_refresh_stats",
                                        ("destination_count", socket_count, i64),
                        );
                    }
                    recv(shutdown_receiver) -> _ => {
                        break;
                    }
                }
        }
    }).unwrap()
}

/// Returns dynamically discovered endpoints with CLI arg defined endpoints
fn fetch_unioned_destinations(
    endpoint_discovery_url: &str,
    discovered_endpoints_port: u16,
    static_dest_sockets: &[(SocketAddr, String)],
) -> Result<Vec<SocketAddr>, ShredstreamProxyError> {
    let bytes = reqwest::blocking::get(endpoint_discovery_url)?.bytes()?;

    let sockets_json = match serde_json::from_slice::<Vec<IpAddr>>(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Failed to parse json from: {:?}",
                std::str::from_utf8(&bytes)
            );
            return Err(ShredstreamProxyError::from(e));
        }
    };

    // resolve again since ip address could change
    let static_dest_sockets = static_dest_sockets
        .iter()
        .filter_map(|(_socketaddr, hostname_port)| {
            Some(resolve_hostname_port(hostname_port).ok()?.0)
        })
        .collect::<Vec<_>>();

    let unioned_dest_sockets = sockets_json
        .into_iter()
        .map(|ip| SocketAddr::new(ip, discovered_endpoints_port))
        .chain(static_dest_sockets)
        .unique()
        .collect::<Vec<SocketAddr>>();
    Ok(unioned_dest_sockets)
}

/// Reset dedup + send metrics to influx
pub fn start_forwarder_accessory_thread(
    deduper: Arc<RwLock<Deduper<2, [u8]>>>,
    metrics: Arc<ShredMetrics>,
    metrics_update_interval_ms: u64,
    shutdown_receiver: Receiver<()>,
    exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    Builder::new()
        .name("ssPxyAccessory".to_string())
        .spawn(move || {
            let metrics_tick =
                crossbeam_channel::tick(Duration::from_millis(metrics_update_interval_ms));
            let deduper_reset_tick = crossbeam_channel::tick(Duration::from_secs(2));
            let mut rng = rand::thread_rng();
            while !exit.load(Ordering::Relaxed) {
                crossbeam_channel::select! {
                    // reset deduper to avoid false positives
                    recv(deduper_reset_tick) -> _ => {
                        deduper
                            .write()
                            .unwrap()
                            .maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE);
                    }

                    // send metrics to influx
                    recv(metrics_tick) -> _ => {
                        metrics.report();
                        metrics.reset();
                    }

                    // handle SIGINT shutdown
                    recv(shutdown_receiver) -> _ => {
                        break;
                    }
                }
            }
        })
        .unwrap()
}

pub struct ShredMetrics {
    // receive stats
    /// Total number of shreds received. Includes duplicates when receiving shreds from multiple regions
    pub received: AtomicU64,
    /// Total number of shreds successfully forwarded, accounting for all destinations
    pub success_forward: AtomicU64,
    /// Total number of shreds failed to forward, accounting for all destinations
    pub fail_forward: AtomicU64,
    /// Number of duplicate shreds received
    pub duplicate: AtomicU64,
    /// (discarded, not discarded), keyed by (source IP, local listen port).
    /// The listen port lets operators distinguish multicast (`--multicast-subscribe-port`)
    /// from unicast (`--src-bind-port`) traffic that shares a source IP.
    pub packets_received: DashMap<(IpAddr, u16), (u64, u64)>,

    // service metrics
    pub enabled_grpc_service: bool,
    /// Configured multicast listen port; used to render the `device` tag on
    /// `shredstream_proxy-receiver_stats` at report time.
    pub multicast_subscribe_port: u16,
    /// Configured multicast device name (e.g. `doublezero1`); used as the
    /// `device` tag for packets received on `multicast_subscribe_port`.
    pub multicast_device: String,
    /// Number of data shreds recovered using coding shreds
    pub recovered_count: AtomicU64,
    /// Number of Solana entries decoded from shreds
    pub entry_count: AtomicU64,
    /// Number of transactions decoded from shreds
    pub txn_count: AtomicU64,
    /// Number of times we couldn't find the previous DATA_COMPLETE_SHRED flag
    pub unknown_start_position_count: AtomicU64,
    /// Number of FEC recovery errors
    pub fec_recovery_error_count: AtomicU64,
    /// Number of bincode Entry deserialization errors
    pub bincode_deserialize_error_count: AtomicU64,
    /// Number of times we couldn't find the previous DATA_COMPLETE_SHRED flag but tried to deshred+deserialize, and failed
    pub unknown_start_position_error_count: AtomicU64,

    // cumulative metrics (persist after reset)
    pub agg_received_cumulative: AtomicU64,
    pub agg_success_forward_cumulative: AtomicU64,
    pub agg_fail_forward_cumulative: AtomicU64,
    pub duplicate_cumulative: AtomicU64,

    /// First-seen vs duplicate-arrival lag tracking (`--measure-dup-lag`).
    /// Histograms are lifetime-cumulative and are NOT cleared by `reset()`.
    pub dup_lag: Option<DupLagTracker>,
}

impl Default for ShredMetrics {
    fn default() -> Self {
        Self::new(false, 0, String::new(), None)
    }
}

impl ShredMetrics {
    pub fn new(
        enabled_grpc_service: bool,
        multicast_subscribe_port: u16,
        multicast_device: String,
        dup_lag: Option<DupLagTracker>,
    ) -> Self {
        Self {
            enabled_grpc_service,
            multicast_subscribe_port,
            multicast_device,
            dup_lag,
            received: Default::default(),
            success_forward: Default::default(),
            fail_forward: Default::default(),
            duplicate: Default::default(),
            packets_received: DashMap::with_capacity(10),
            recovered_count: Default::default(),
            entry_count: Default::default(),
            txn_count: Default::default(),
            unknown_start_position_count: Default::default(),
            fec_recovery_error_count: Default::default(),
            bincode_deserialize_error_count: Default::default(),
            unknown_start_position_error_count: Default::default(),
            agg_received_cumulative: Default::default(),
            agg_success_forward_cumulative: Default::default(),
            agg_fail_forward_cumulative: Default::default(),
            duplicate_cumulative: Default::default(),
        }
    }

    pub fn report(&self) {
        datapoint_info!(
            "shredstream_proxy-connection_metrics",
            ("received", self.received.load(Ordering::Relaxed), i64),
            (
                "success_forward",
                self.success_forward.load(Ordering::Relaxed),
                i64
            ),
            (
                "fail_forward",
                self.fail_forward.load(Ordering::Relaxed),
                i64
            ),
            ("duplicate", self.duplicate.load(Ordering::Relaxed), i64),
        );

        if self.enabled_grpc_service {
            datapoint_info!(
                "shredstream_proxy-service_metrics",
                (
                    "recovered_count",
                    self.recovered_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "entry_count",
                    self.entry_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                ("txn_count", self.txn_count.swap(0, Ordering::Relaxed), i64),
                (
                    "unknown_start_position_count",
                    self.unknown_start_position_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "fec_recovery_error_count",
                    self.fec_recovery_error_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "bincode_deserialize_error_count",
                    self.bincode_deserialize_error_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "unknown_start_position_error_count",
                    self.unknown_start_position_error_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
            );
        }

        self.packets_received.retain(
            |(addr, listen_port), (discarded_packets, not_discarded_packets)| {
                let device = if *listen_port == self.multicast_subscribe_port {
                    self.multicast_device.as_str()
                } else {
                    "unicast"
                };
                datapoint_info!("shredstream_proxy-receiver_stats",
                    "addr" => addr.to_string(),
                    "listen_port" => listen_port.to_string(),
                    "device" => device,
                    ("discarded_packets", *discarded_packets, i64),
                    ("not_discarded_packets", *not_discarded_packets, i64),
                );
                false
            },
        );

        // Lifetime-cumulative Prometheus-style histogram per (winner, loser) source pair.
        // Telegraf turns this into shredstream_proxy_dup_lag_seconds_bucket{le,...}/_count/_sum.
        if let Some(dup_lag) = &self.dup_lag {
            let device_for = |port: u16| {
                if port == self.multicast_subscribe_port {
                    self.multicast_device.as_str()
                } else {
                    "unicast"
                }
            };
            for entry in dup_lag.hist.iter() {
                let (winner_addr, winner_port, loser_addr, loser_port) = *entry.key();
                let hist = entry.value();
                let mut cumulative = 0u64;
                for (i, le) in DUP_LAG_LE_SECONDS.iter().enumerate() {
                    cumulative += hist.buckets[i].load(Ordering::Relaxed);
                    datapoint_info!("shredstream_proxy-dup_lag_seconds",
                        "winner_addr" => winner_addr.to_string(),
                        "winner_port" => winner_port.to_string(),
                        "winner_device" => device_for(winner_port),
                        "loser_addr" => loser_addr.to_string(),
                        "loser_port" => loser_port.to_string(),
                        "loser_device" => device_for(loser_port),
                        "le" => *le,
                        ("bucket", cumulative, i64),
                    );
                }
                let count = hist.count.load(Ordering::Relaxed);
                datapoint_info!("shredstream_proxy-dup_lag_seconds",
                    "winner_addr" => winner_addr.to_string(),
                    "winner_port" => winner_port.to_string(),
                    "winner_device" => device_for(winner_port),
                    "loser_addr" => loser_addr.to_string(),
                    "loser_port" => loser_port.to_string(),
                    "loser_device" => device_for(loser_port),
                    "le" => "+Inf",
                    ("bucket", count, i64),
                );
                datapoint_info!("shredstream_proxy-dup_lag_seconds",
                    "winner_addr" => winner_addr.to_string(),
                    "winner_port" => winner_port.to_string(),
                    "winner_device" => device_for(winner_port),
                    "loser_addr" => loser_addr.to_string(),
                    "loser_port" => loser_port.to_string(),
                    "loser_device" => device_for(loser_port),
                    ("count", count, i64),
                    (
                        "sum",
                        hist.sum_micros.load(Ordering::Relaxed) as f64 / 1e6,
                        f64
                    ),
                );
            }
        }
    }

    /// resets current values, increments cumulative values
    pub fn reset(&self) {
        self.agg_received_cumulative
            .fetch_add(self.received.swap(0, Ordering::Relaxed), Ordering::Relaxed);
        self.agg_success_forward_cumulative.fetch_add(
            self.success_forward.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.agg_fail_forward_cumulative.fetch_add(
            self.fail_forward.swap(0, Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.duplicate_cumulative
            .fetch_add(self.duplicate.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        str::FromStr,
        sync::{Arc, Mutex, RwLock},
        thread,
        thread::sleep,
        time::Duration,
    };

    use solana_perf::{
        deduper::Deduper,
        packet::{Meta, Packet, PacketBatch},
    };
    use solana_sdk::packet::{PacketFlags, PACKET_DATA_SIZE};

    use std::sync::atomic::Ordering;

    use crate::forwarder::{recv_from_channel_and_send_multiple_dest, DupLagTracker, ShredMetrics};

    fn listen_and_collect(listen_socket: UdpSocket, received_packets: Arc<Mutex<Vec<Vec<u8>>>>) {
        let mut buf = [0u8; PACKET_DATA_SIZE];
        loop {
            listen_socket.recv(&mut buf).unwrap();
            received_packets.lock().unwrap().push(Vec::from(buf));
        }
    }

    #[test]
    fn test_2shreds_3destinations() {
        let packet_batch = PacketBatch::new(vec![
            Packet::new(
                [1; PACKET_DATA_SIZE],
                Meta {
                    size: PACKET_DATA_SIZE,
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 48289, // received on random port
                    flags: PacketFlags::empty(),
                },
            ),
            Packet::new(
                [2; PACKET_DATA_SIZE],
                Meta {
                    size: PACKET_DATA_SIZE,
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 9999,
                    flags: PacketFlags::empty(),
                },
            ),
        ]);
        let (packet_sender, packet_receiver) = crossbeam_channel::unbounded::<PacketBatch>();
        packet_sender.send(packet_batch).unwrap();

        let dest_socketaddrs = vec![
            SocketAddr::from_str("127.0.0.1:32881").unwrap(),
            SocketAddr::from_str("127.0.0.1:33881").unwrap(),
            SocketAddr::from_str("127.0.0.1:34881").unwrap(),
        ];

        let test_listeners = dest_socketaddrs
            .iter()
            .map(|socketaddr| {
                (
                    UdpSocket::bind(socketaddr).unwrap(),
                    *socketaddr,
                    // store results in vec of packet, where packet is Vec<u8>
                    Arc::new(Mutex::new(vec![])),
                )
            })
            .collect::<Vec<_>>();

        let udp_sender = UdpSocket::bind("127.0.0.1:10000").unwrap();

        // spawn listeners
        test_listeners
            .iter()
            .for_each(|(listen_socket, _socketaddr, to_receive)| {
                let socket = listen_socket.try_clone().unwrap();
                let to_receive = to_receive.to_owned();
                thread::spawn(move || listen_and_collect(socket, to_receive));
            });

        let (reconstruct_tx, _reconstruct_rx) = crossbeam_channel::bounded(10_240);
        // send packets
        recv_from_channel_and_send_multiple_dest(
            packet_receiver.recv(),
            20_000,
            &Arc::new(RwLock::new(Deduper::<2, [u8]>::new(
                &mut rand::thread_rng(),
                crate::forwarder::DEDUPER_NUM_BITS,
            ))),
            &udp_sender,
            &Arc::new(dest_socketaddrs),
            true,
            &reconstruct_tx,
            false,
            &Arc::new(ShredMetrics::default()),
        )
        .unwrap();

        // allow packets to be received
        sleep(Duration::from_millis(500));

        let received = test_listeners
            .iter()
            .map(|(_, _, results)| results.clone())
            .collect::<Vec<_>>();

        // check results
        for received in received.iter() {
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 2);
            assert!(received
                .iter()
                .all(|packet| packet.len() == PACKET_DATA_SIZE));
            assert_eq!(received[0], [1; PACKET_DATA_SIZE]);
            assert_eq!(received[1], [2; PACKET_DATA_SIZE]);
        }

        assert_eq!(
            received
                .iter()
                .fold(0, |acc, elem| acc + elem.lock().unwrap().len()),
            6
        );
    }

    #[test]
    fn test_dup_lag_records_winner_loser_pair() {
        let tracker = DupLagTracker::new(Duration::from_secs(10), 1);
        let now = std::time::Instant::now();
        let jito = IpAddr::V4(Ipv4Addr::new(202, 8, 9, 160));
        let dz = IpAddr::V4(Ipv4Addr::new(148, 51, 121, 14));
        let shred = [42u8; 1228];

        // first arrival: DZ multicast wins
        tracker.observe(&shred, now, dz, 7733);
        assert!(tracker.hist.is_empty(), "first sighting must not record");

        // duplicate 2.4ms later via unicast: (winner=dz, loser=jito) in the 2.5ms bucket
        tracker.observe(&shred, now + Duration::from_micros(2_400), jito, 20000);
        let hist = tracker
            .hist
            .get(&(dz, 7733, jito, 20000))
            .expect("pair recorded with winner first");
        assert_eq!(hist.count.load(Ordering::Relaxed), 1);
        assert_eq!(hist.sum_micros.load(Ordering::Relaxed), 2_400);
        // bands: index 5 is (1ms, 2.5ms]
        assert_eq!(hist.buckets[5].load(Ordering::Relaxed), 1);
        assert_eq!(
            (0..13)
                .map(|i| hist.buckets[i].load(Ordering::Relaxed))
                .sum::<u64>(),
            1
        );

        // different bytes are a different shred: no cross-recording
        let other = [7u8; 1228];
        tracker.observe(&other, now, jito, 20000);
        assert_eq!(tracker.hist.len(), 1);
    }

    #[test]
    fn test_dup_lag_overflow_bucket() {
        let tracker = DupLagTracker::new(Duration::from_secs(10), 1);
        let now = std::time::Instant::now();
        let a = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let b = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));
        let shred = [9u8; 100];
        tracker.observe(&shred, now, a, 1);
        tracker.observe(&shred, now + Duration::from_secs(2), b, 2); // > 500ms => +Inf band
        let hist = tracker.hist.get(&(a, 1, b, 2)).unwrap();
        assert_eq!(hist.count.load(Ordering::Relaxed), 1);
        assert_eq!(hist.overflow.load(Ordering::Relaxed), 1);
        assert_eq!(
            (0..13)
                .map(|i| hist.buckets[i].load(Ordering::Relaxed))
                .sum::<u64>(),
            0
        );
    }

    #[test]
    fn test_dup_lag_rotation_evicts_first_seen() {
        let tracker = DupLagTracker::new(Duration::from_millis(1), 1);
        let now = std::time::Instant::now();
        let a = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let b = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));
        let shred = [5u8; 100];

        tracker.observe(&shred, now, a, 1);
        // two rotations later (entry aged out of both shards), the same bytes
        // are treated as a fresh first arrival, not a duplicate
        tracker.maybe_rotate(now + Duration::from_millis(2));
        tracker.maybe_rotate(now + Duration::from_millis(4));
        tracker.observe(&shred, now + Duration::from_millis(5), b, 2);
        assert!(
            tracker.hist.is_empty(),
            "expired first-seen must not produce a lag record"
        );
    }

    #[test]
    fn test_dup_lag_idle_gap_drops_both_shards() {
        // After an idle gap far beyond the retention window, a re-arrival of
        // old bytes must be a fresh first-seen, not a multi-second "lag".
        let tracker = DupLagTracker::new(Duration::from_millis(10), 1);
        let now = std::time::Instant::now();
        let a = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let b = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));
        let shred = [3u8; 100];

        tracker.observe(&shred, now, a, 1);
        // single observe after a 50ms gap (5 retention periods): the one
        // rotation it triggers must clear BOTH shards
        tracker.observe(&shred, now + Duration::from_millis(50), b, 2);
        assert!(
            tracker.hist.is_empty(),
            "stale first-seen across an idle gap must not record a lag"
        );
        // and the re-arrival was stored as the new first-seen: an immediate
        // duplicate now records with it as the winner
        tracker.observe(&shred, now + Duration::from_millis(51), a, 1);
        assert!(tracker.hist.get(&(b, 2, a, 1)).is_some());
    }

    #[test]
    fn test_dup_lag_rotation_runs_on_unsampled_traffic() {
        // With sampling enabled, unsampled packets must still drive rotation
        // so first-seen retention is bounded by time, not by sampled arrivals.
        let tracker = DupLagTracker::new_seeded(Duration::from_millis(10), 4, 99);
        let now = std::time::Instant::now();
        let a = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));

        let sampled = (0..=255u8)
            .map(|i| [i; 64])
            .find(|p| tracker.hasher.hash_one(p.as_slice()) & 3 == 0)
            .expect("some payload samples");
        let unsampled = (0..=255u8)
            .map(|i| [i; 64])
            .find(|p| tracker.hasher.hash_one(p.as_slice()) & 3 != 0)
            .expect("some payload does not sample");

        tracker.observe(&sampled, now, a, 1);
        assert_eq!(tracker.shards[0].len() + tracker.shards[1].len(), 1);
        // an UNSAMPLED arrival after a 5-period gap must still rotate (and,
        // having overshot a full period, clear both shards)
        tracker.observe(&unsampled, now + Duration::from_millis(50), a, 1);
        assert_eq!(
            tracker.shards[0].len() + tracker.shards[1].len(),
            0,
            "unsampled traffic must drive rotation"
        );
    }

    #[test]
    fn test_dup_lag_hash_sampling_keeps_pairs_consistent() {
        // mask=3 samples 1-in-4 by hash; a sampled shred must record its pair
        // (both arrivals observed), an unsampled one must record nothing.
        let tracker = DupLagTracker::new_seeded(Duration::from_secs(10), 4, 1234);
        let now = std::time::Instant::now();
        let a = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        let b = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));

        let mut sampled = 0u64;
        for i in 0..64u8 {
            let payload = [i; 64];
            let expect_sampled = tracker.hasher.hash_one(payload.as_slice()) & 3 == 0;
            tracker.observe(&payload, now, a, 1);
            tracker.observe(&payload, now + Duration::from_micros(100), b, 2);
            sampled += expect_sampled as u64;
        }
        assert!(sampled > 0, "seed should sample at least one of 64 payloads");
        let hist = tracker.hist.get(&(a, 1, b, 2)).expect("sampled pairs recorded");
        assert_eq!(hist.count.load(Ordering::Relaxed), sampled);
    }
}
