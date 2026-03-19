// GROUND CONTROL STATION - BY CHONG CHUN KIT (TP077436)
// SOFT RTS

// =============================================================================
//  src/bin/GCS.rs   —   Ground Control Station (GCS)
//  CT087-3-3 Real-Time Systems  |  Student B
//
//  Run:   cargo run --bin GCS --release
//
//  Architecture: Soft Real-Time System on Tokio cooperative multitasking.
//  Communicates with the OCS exclusively via MQTT (the IPC channel).
//
//  Tutorial map — every major pattern traced to its source lab:
//  ─────────────────────────────────────────────────────────────
//  Lab 1  const, structs, enums, match, Vec, for, mut
//  Lab 2  Arc<Mutex<T>>, Arc::clone(), jitter / drift formula
//  Lab 3  Instant::now() + .elapsed(), min/max/avg stat helper
//  Lab 6  #[tokio::main], tokio::spawn, sleep().await
//  Lab 7  enum error variants + #[derive(Debug)], match arms,
//         consecutive-miss counter that resets on success
//  Lab 8  tokio::select!, CancellationToken, supervisor restart loop,
//         fragile uplink + match handle.await { Err(e) if e.is_panic() }
//  Lab 9  MqttOptions::new, AsyncClient::new, eventloop.poll() spawn,
//         client.subscribe(), client.publish() for command uplink
//  Lab 11 Arc<Mutex<Vec>> command queue (job-queue pattern),
//         mpsc channel for MQTT-to-decoder pipeline
// =============================================================================

// ── imports ───────────────────────────────────────────────────────────────────
use std::collections::VecDeque;         // lab 1 — data structures
use std::sync::{Arc, Mutex};            // lab 2 — Arc<Mutex<T>>
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH}; // lab 3

use tokio::time::sleep;                  // lab 6 — non-blocking sleep
use tokio::sync::mpsc;                  // lab 11 — async mpsc channel
use tokio_util::sync::CancellationToken; // lab 8 — graceful shutdown

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS}; // lab 9 — MQTT

use rand; // lab 7 — rand::random_range for fault simulation

// =============================================================================
//  PART 0 — CONSTANTS   (lab 1 — const)
// =============================================================================

// GCS-side timing requirements (from assignment)
const DECODE_DEADLINE_MS:      u64   = 3;      // telemetry must decode within 3ms
const CMD_SCHEDULE_PERIOD_MS:  u64   = 500;    // uplink supervisor tick
const HEALTH_PERIOD_MS:        u64   = 200;    // GCS health monitor  RM P1
const MONITOR_PERIOD_MS:       u64   = 1_000;  // uplink monitor      RM P2
const ALERT_PERIOD_MS:         u64   = 2_000;  // fault scan          RM P3

// Fault management thresholds
const GROUND_ALERT_LIMIT_MS:   u64   = 100;    // fault recovery > this → critical alert
const BACKLOG_WARN_THRESHOLD:  usize = 20;      // unprocessed pkt count → warn

// Jitter ceiling for GCS tasks
const JITTER_LIMIT_US:         i64   = 2_000;  // 2ms

// Simulation
const SIM_DURATION_S: u64 = 180;

// MQTT — same broker as lab 9, same STUDENT_ID as OCS
const MQTT_BROKER: &str = "broker.emqx.io";
const MQTT_PORT:   u16  = 1883;
const STUDENT_ID:  &str = "tp071542"; // must match OCS STUDENT_ID exactly

// =============================================================================
//  PART 0 — DATA TYPES   (lab 1 — structs, enums)
// =============================================================================

// One decoded telemetry packet from OCS
#[derive(Debug, Clone)]
struct TelemetryPacket
{
    source:       String,  // "thermal" | "accel" | "gyro" | "status" | "downlink"
    raw_payload:  String,
    received_ms:  u64,     // UNIX ms of MQTT arrival
    decoded_ms:   u64,     // UNIX ms after decoding
    sequence:     u64,
}

// A command queued for uplink to OCS  (lab 1 — struct)
#[derive(Debug, Clone)]
struct Command
{
    id:          u64,
    command:     String,   // "HEARTBEAT", "ADJUST_ANTENNA", etc.
    priority:    u8,       // 1 = highest
    deadline_ms: u64,      // expire time — UNIX ms
}

// GCS interlock state.  Valid transitions: Clear -> FaultActive -> CriticalAlert
// (lab 7 typestate concept expressed as a runtime enum because async tasks
//  cannot own generic PhantomData<S> structs)
#[derive(Debug, Clone, PartialEq)]
enum InterlockState
{
    Clear,          // no active fault, commands may uplink
    FaultActive,    // OCS reported a fault, block commands
    CriticalAlert,  // recovery exceeded 100ms, hard block
}

// Fault classification received from OCS via MQTT  (lab 7 — SensorError mirror)
#[derive(Debug)]
enum FaultAlert
{
    ThermalAlert  { misses: u32 },
    GyroRestart   { generation: u32 },
    FaultInjected { type_str: String },
}

// Result of an uplink attempt  (lab 7 — Result / enum pattern)
#[derive(Debug)]
enum UplinkResult
{
    Sent,
    Blocked(String),   // reason string
    DeadlineMissed,
}

// Internal MQTT-to-decoder message  (lab 11 — Job struct equivalent)
struct RawMqttMsg
{
    topic:      String,
    payload:    String,
    arrival_ms: u64,
}

// Shared GCS metrics  (lab 2 — Arc<Mutex<T>>)
#[derive(Default)]
struct GcsMetrics
{
    // Telemetry pipeline
    pkts_received:        u64,
    pkts_decoded:         u64,
    decode_latency_us:    Vec<u64>,
    reception_drift_ms:   Vec<i64>,  // actual interval − expected

    // Backlog depth snapshots (lab 11 — queue utilisation)
    backlog_samples: Vec<usize>,

    // Link jitter  (inter-packet arrival variance)
    link_jitter_us:       Vec<i64>,
    last_pkt_arrival_ms:  u64,

    // Command uplink
    cmds_sent:            u64,
    cmds_blocked:         u64,
    cmds_deadline_miss:   u64,
    cmd_log:              Vec<String>,

    // Fault management
    faults_received:      u64,
    fault_recovery_ms:    Vec<u64>,
    interlock_latency_ms: Vec<u64>,
    ground_alerts:        Vec<String>,

    // Task jitter / drift for GCS RM tasks
    task_jitter_us: Vec<i64>,
    task_drift_ms:  Vec<i64>,

    // Lab 8 supervisor count
    uplink_restarts: u32,
}

// =============================================================================
//  HELPERS
// =============================================================================

fn now_ms() -> u64
{
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

// min/max/avg row — mirrors lab 3's print_stats helper
fn print_stat_row(label: &str, samples: &[i64])
{
    if samples.is_empty()
    {
        println!("    {:<38} — no data", label);
        return;
    }
    let min = samples.iter().min().unwrap();
    let max = samples.iter().max().unwrap();
    let avg = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
    println!(
        "    {:<38} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}",
        label, samples.len(), min, max, avg
    );
}

// =============================================================================
//  MQTT SETUP   (lab 9)
//
//  GCS is the subscriber side.  Subscribes to all OCS publish topics.
//  Returns the client handle so tasks can also publish uplink commands.
// =============================================================================

async fn setup_mqtt(shutdown: CancellationToken) -> AsyncClient
{
    // Unique client ID — same rule as lab 9
    let client_id = format!("gcs_{}_{}", STUDENT_ID, now_ms() % 100_000);

    // Steps 1 & 2  (lab 9)
    let mut mqttoptions = MqttOptions::new(client_id, MQTT_BROKER, MQTT_PORT);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    // Step 3  (lab 9)
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 50);

    // Subscribe to every OCS publish topic  (lab9_server.rs subscribe pattern)
    for suffix in ["telemetry/thermal", "telemetry/accel", "telemetry/gyro",
                   "status", "downlink", "alerts"]
    {
        let topic = format!("ocs/{}/{}", STUDENT_ID, suffix);
        client.subscribe(&topic, QoS::AtMostOnce).await
              .unwrap_or_else(|e| println!("[MQTT] Subscribe error '{}': {e}", topic));
        println!("[MQTT] Subscribed → '{topic}'");
    }

    println!("[MQTT] Uplink commands → gcs/{STUDENT_ID}/commands");

    // Step 4  (lab 9) — event loop must stay alive; add lab 8 shutdown arm
    tokio::spawn(async move
    {
        loop
        {
            tokio::select!
            {
                _ = shutdown.cancelled() => { println!("[MQTT] Event loop exit."); return; }
                result = eventloop.poll() =>
                {
                    match result
                    {
                        Ok(_)  => {}
                        Err(e) =>
                        {
                            println!("[MQTT] Connection error: {e}. Retrying in 2s...");
                            sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    });

    client
}

// =============================================================================
//  MQTT RECEIVER TASK   (lab 9 subscriber + lab 11 mpsc producer)
//
//  Opens a second MQTT connection dedicated to receiving OCS publishes.
//  Forwards each raw message to the telemetry decoder task via mpsc.
//  This is the "producer" half of the lab 11 channel pattern.
// =============================================================================

async fn mqtt_receiver_task(
    mqtt_tx:  mpsc::Sender<RawMqttMsg>, // lab 11 — producer sends to decoder
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[MQTT-RX]  Receiver online.");

    // Second connection for incoming data
    let rx_id = format!("gcs_rx_{}_{}", STUDENT_ID, now_ms() % 100_000);
    let mut opts = MqttOptions::new(rx_id, MQTT_BROKER, MQTT_PORT);
    opts.set_keep_alive(Duration::from_secs(5));
    let (rx_client, mut eventloop) = AsyncClient::new(opts, 50);

    for suffix in ["telemetry/thermal", "telemetry/accel", "telemetry/gyro",
                   "status", "downlink", "alerts"]
    {
        rx_client.subscribe(
            &format!("ocs/{}/{}", STUDENT_ID, suffix),
            QoS::AtMostOnce
        ).await.ok();
    }

    loop
    {
        // lab 8 — select! races shutdown vs network event
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[MQTT-RX]  Exit."); return; }
            result = eventloop.poll() =>
            {
                match result
                {
                    Ok(Event::Incoming(Packet::Publish(p))) =>
                    {
                        let arrival_ms = now_ms();
                        let payload    = String::from_utf8_lossy(&p.payload).to_string();

                        // Link jitter — time since last packet  (lab 2 jitter formula)
                        {
                            let mut m = metrics.lock().unwrap();
                            if m.last_pkt_arrival_ms > 0
                            {
                                let gap_ms   = arrival_ms as i64 - m.last_pkt_arrival_ms as i64;
                                let jit_us   = (gap_ms * 1_000).unsigned_abs() as i64;
                                m.link_jitter_us.push(jit_us);
                            }
                            m.last_pkt_arrival_ms = arrival_ms;
                            m.pkts_received      += 1;
                        }

                        // lab 11 — send to consumer (telemetry_decoder_task)
                        let _ = mqtt_tx.try_send(RawMqttMsg
                        {
                            topic:      p.topic.clone(),
                            payload,
                            arrival_ms,
                        });
                    }
                    Ok(_)  => {}
                    Err(e) =>
                    {
                        println!("[MQTT-RX]  Error: {e}. Retrying...");
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }
}

// =============================================================================
//  PART 1 — TELEMETRY RECEPTION AND DECODING
//
//  Drains the mpsc channel (lab 11 consumer), decodes each packet within
//  the 3ms assignment deadline (lab 3 timing), measures reception drift
//  (lab 2 formula), and logs any deadline misses.
// =============================================================================

// Parse fault alerts and update interlock state  (lab 7 — match on enum)
fn handle_alert_payload(
    payload:   &str,
    interlock: &Arc<Mutex<InterlockState>>,
    metrics:   &Arc<Mutex<GcsMetrics>>,
)
{
    // Manual JSON parse — no serde (tutorial-aligned, same style as lab9 payloads)
    // lab 7 — match on FaultAlert variants, mirrors match on SensorError
    let alert: Option<FaultAlert> =
        if payload.contains("thermal_alert")
        {
            let misses = payload.split("misses\":").nth(1)
                .and_then(|s| s.split('}').next())
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            Some(FaultAlert::ThermalAlert { misses })
        }
        else if payload.contains("gyro_restart")
        {
            let generation = payload.split("generation\":").nth(1)
                .and_then(|s| s.split('}').next())
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            Some(FaultAlert::GyroRestart { generation: generation })
        }
        else if payload.contains("\"event\":\"fault\"")
        {
            let type_str = payload.split("\"type\":\"").nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("Unknown")
                .to_string();
            Some(FaultAlert::FaultInjected { type_str })
        }
        else { None };

    if let Some(a) = alert
    {
        let msg = format!("[{}ms] [ALERT] Received from OCS: {:?}", now_ms(), a);
        println!("{msg}");
        let mut m = metrics.lock().unwrap();
        m.faults_received += 1;
        m.cmd_log.push(msg);

        // Engage interlock — block commands until resolved
        let mut il = interlock.lock().unwrap();
        if *il == InterlockState::Clear { *il = InterlockState::FaultActive; }
    }
}

async fn telemetry_decoder_task(
    mut rx:    mpsc::Receiver<RawMqttMsg>, // lab 11 — mpsc consumer
    backlog:   Arc<Mutex<VecDeque<TelemetryPacket>>>,
    metrics:   Arc<Mutex<GcsMetrics>>,
    interlock: Arc<Mutex<InterlockState>>,
    shutdown:  CancellationToken,
)
{
    println!("[Decoder]  Telemetry decoder  decode_limit={}ms", DECODE_DEADLINE_MS);
    let mut seq:            u64 = 0;
    let mut last_arrival:   u64 = 0;

    loop
    {
        // lab 8 — select! races shutdown vs incoming message
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Decoder]  Exit."); return; }
            msg = rx.recv() =>
            {
                match msg
                {
                    None => return, // channel closed
                    Some(raw) =>
                    {
                        // lab 3 — time the decode with Instant::now()
                        let decode_t0 = Instant::now();

                        // Reception drift  (lab 2 — expected vs actual interval)
                        let drift_ms = if last_arrival > 0
                            { raw.arrival_ms as i64 - last_arrival as i64 }
                            else { 0 };
                        last_arrival = raw.arrival_ms;

                        // Check for alerts first — fastest path to interlock
                        if raw.topic.ends_with("/alerts")
                        {
                            handle_alert_payload(&raw.payload, &interlock, &metrics);
                        }

                        let source = raw.topic
                            .rsplit('/')
                            .next()
                            .unwrap_or("unknown")
                            .to_string();

                        seq += 1;
                        let pkt = TelemetryPacket
                        {
                            source:      source.clone(),
                            raw_payload: raw.payload.clone(),
                            received_ms: raw.arrival_ms,
                            decoded_ms:  now_ms(),
                            sequence:    seq,
                        };

                        let decode_us  = decode_t0.elapsed().as_micros() as u64;
                        let decode_ms  = decode_t0.elapsed().as_millis() as u64;

                        println!(
                            "[Decoder]  seq={seq:>5}  src={source:<12}  \
                             decode={}µs  drift={drift_ms:+}ms",
                            decode_us
                        );

                        // Log deadline miss if decode took > 3ms
                        {
                            let mut m = metrics.lock().unwrap();
                            m.pkts_decoded       += 1;
                            m.decode_latency_us.push(decode_us);
                            m.reception_drift_ms.push(drift_ms);

                            if decode_ms > DECODE_DEADLINE_MS
                            {
                                let v = format!(
                                    "[{}ms] [DEADLINE] Decode {}ms > limit {}ms  seq={seq}",
                                    now_ms(), decode_ms, DECODE_DEADLINE_MS
                                );
                                println!("{v}");
                                m.cmd_log.push(v);
                            }
                        }

                        // Push to backlog  (lab 11 — shared Arc<Mutex<Vec>> job queue)
                        backlog.lock().unwrap().push_back(pkt);
                    }
                }
            }
        }
    }
}

// =============================================================================
//  PART 2 — COMMAND UPLINK SCHEDULE
//
//  RM priority for GCS background tasks:
//    gcs_health_monitor  200ms  RM P1
//    uplink_monitor      1000ms RM P2
//    fault_manager       2000ms RM P3  (preemptible via select!)
//
//  Every command is validated against InterlockState before uplink —
//  the lab 7 "only valid transitions are permitted" principle expressed
//  as a runtime match rather than compile-time PhantomData typestates.
// =============================================================================

// ── Lab 8 Part 1 — fragile uplink task ────────────────────────────────────────
// Direct equivalent of fragile_worker() in lab 8.
// Does real uplink work but has a small random chance to panic,
// simulating a radio hardware fault or driver crash.
async fn fragile_uplink(
    generation:  u32,
    cmd_queue:   Arc<Mutex<Vec<Command>>>,
    metrics:     Arc<Mutex<GcsMetrics>>,
    interlock:   Arc<Mutex<InterlockState>>,
    mqtt_client: AsyncClient,
)
{
    println!("[Uplink-{generation}]  Started  period={}ms", CMD_SCHEDULE_PERIOD_MS);
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        sleep(Duration::from_millis(CMD_SCHEDULE_PERIOD_MS)).await;

        // lab 7 — rand::random_range, lab 8 — 2% panic chance (fragile_worker)
        if rand::random_range(0u32..100) < 2
        {
            println!("[Uplink-{generation}]  Radio hardware fault — panicking!");
            panic!("Uplink radio fault (generation {generation})");
        }

        let expected_ms = iter * CMD_SCHEDULE_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Check interlock state — lab 7 match on InterlockState enum
        let il_state = interlock.lock().unwrap().clone();

        // Drain one command per tick  (lab 11 — queue.lock().unwrap().pop())
        let cmd = cmd_queue.lock().unwrap().pop();

        if let Some(c) = cmd
        {
            let now = now_ms();

            // lab 7 — match on state, same pattern as match read_sensor()
            let result = match il_state
            {
                InterlockState::Clear =>
                {
                    if now > c.deadline_ms
                    {
                        UplinkResult::DeadlineMissed
                    }
                    else
                    {
                        // lab 9 — publish command to OCS
                        let topic   = format!("gcs/{}/commands", STUDENT_ID);
                        let payload = format!(
                            "{{\"id\":{},\"cmd\":\"{}\",\"prio\":{}}}",
                            c.id, c.command, c.priority
                        );
                        match mqtt_client
                            .publish(&topic, QoS::AtMostOnce, false, payload.as_bytes())
                            .await
                        {
                            Ok(_)  => UplinkResult::Sent,
                            Err(e) => UplinkResult::Blocked(format!("publish error: {e}")),
                        }
                    }
                }
                InterlockState::FaultActive =>
                {
                    UplinkResult::Blocked("interlock: fault active".into())
                }
                InterlockState::CriticalAlert =>
                {
                    UplinkResult::Blocked("interlock: CRITICAL ALERT".into())
                }
            };

            // lab 7 — match on UplinkResult, mirrors match on SensorError
            {
                let mut m = metrics.lock().unwrap();
                match &result
                {
                    UplinkResult::Sent =>
                    {
                        m.cmds_sent += 1;
                        println!(
                            "[Uplink-{generation}]  CMD #{} '{}' sent  drift={drift_ms:+}ms",
                            c.id, c.command
                        );
                    }
                    UplinkResult::Blocked(reason) =>
                    {
                        m.cmds_blocked += 1;
                        let log = format!(
                            "[{}ms] [BLOCKED] CMD #{} '{}' — {reason}",
                            now_ms(), c.id, c.command
                        );
                        println!("{log}");
                        m.cmd_log.push(log);
                    }
                    UplinkResult::DeadlineMissed =>
                    {
                        m.cmds_deadline_miss += 1;
                        let log = format!(
                            "[{}ms] [DEADLINE] CMD #{} '{}' expired",
                            now_ms(), c.id, c.command
                        );
                        println!("{log}");
                        m.cmd_log.push(log);
                    }
                }

                m.task_drift_ms.push(drift_ms);
                if drift_ms.abs() > 5
                {
                    let jit_us = (drift_ms * 1_000).unsigned_abs() as i64;
                    m.task_jitter_us.push(jit_us);
                    if jit_us > JITTER_LIMIT_US
                    {
                        let v = format!(
                            "[{}ms] [WARN] Uplink jitter {}µs > {}µs limit",
                            now_ms(), jit_us, JITTER_LIMIT_US
                        );
                        println!("{v}");
                        m.cmd_log.push(v);
                    }
                }
            }
        }

        iter += 1;
    }
}

// ── Lab 8 Part 2 — uplink supervisor ──────────────────────────────────────────
// Mirrors the supervisor loop from lab 8 Part 2.
// Restarts fragile_uplink on panic with a backoff.
async fn uplink_supervisor_task(
    cmd_queue:   Arc<Mutex<Vec<Command>>>,
    metrics:     Arc<Mutex<GcsMetrics>>,
    interlock:   Arc<Mutex<InterlockState>>,
    mqtt_client: AsyncClient,
    shutdown:    CancellationToken,
)
{
    println!("[UL-SUP]   Uplink supervisor online.");
    let mut generation: u32 = 0;

    loop
    {
        if shutdown.is_cancelled() { println!("[UL-SUP]   Exit."); return; }

        generation += 1;
        println!("[UL-SUP]   Starting uplink generation {generation}...");

        // lab 8 — spawn + await
        let handle = tokio::spawn(fragile_uplink(
            generation,
            Arc::clone(&cmd_queue),  // lab 2 — Arc::clone
            Arc::clone(&metrics),
            Arc::clone(&interlock),
            mqtt_client.clone(),
        ));

        // lab 8 — match handle.await { Err(e) if e.is_panic() }
        match handle.await
        {
            Ok(_) => { println!("[UL-SUP]   Uplink exited normally."); return; }
            Err(e) if e.is_panic() =>
            {
                metrics.lock().unwrap().uplink_restarts += 1;
                println!("[UL-SUP]   Uplink panicked! Restarting in 1s...");
                sleep(Duration::from_secs(1)).await; // backoff — lab 8 pattern
            }
            Err(_) => { println!("[UL-SUP]   Uplink cancelled — exit."); return; }
        }
    }
}

// =============================================================================
//  GCS HEALTH MONITOR   (RM P1 — 200ms)
// =============================================================================

async fn gcs_health_monitor_task(
    backlog:   Arc<Mutex<VecDeque<TelemetryPacket>>>,
    metrics:   Arc<Mutex<GcsMetrics>>,
    interlock: Arc<Mutex<InterlockState>>,
    shutdown:  CancellationToken,
)
{
    println!("[GCS-HLT]  Health monitor  period={}ms  RM P1", HEALTH_PERIOD_MS);
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        // lab 8 — select!
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[GCS-HLT]  Exit."); return; }
            _ = sleep(Duration::from_millis(HEALTH_PERIOD_MS)) => {}
        }

        let expected_ms = iter * HEALTH_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        let depth    = backlog.lock().unwrap().len();
        let il_state = interlock.lock().unwrap().clone();

        println!(
            "[GCS-HLT]  iter={iter:>4}  backlog={depth:>3}  \
             interlock={il_state:?}  drift={drift_ms:+}ms"
        );

        {
            let mut m = metrics.lock().unwrap();
            m.backlog_samples.push(depth);
            m.task_drift_ms.push(drift_ms);
            if drift_ms.abs() > 5
            {
                m.task_jitter_us.push((drift_ms * 1_000).unsigned_abs() as i64);
            }
        }

        if depth >= BACKLOG_WARN_THRESHOLD
        {
            println!("[GCS-HLT]  WARNING: backlog {} >= warn threshold {}",
                depth, BACKLOG_WARN_THRESHOLD);
        }

        iter += 1;
    }
}

// =============================================================================
//  UPLINK MONITOR   (RM P2 — 1000ms)
//  Generates periodic command schedule and enqueues validated commands.
// =============================================================================

async fn uplink_monitor_task(
    cmd_queue:  Arc<Mutex<Vec<Command>>>,
    backlog:    Arc<Mutex<VecDeque<TelemetryPacket>>>,
    metrics:    Arc<Mutex<GcsMetrics>>,
    interlock:  Arc<Mutex<InterlockState>>,
    shutdown:   CancellationToken,
)
{
    println!("[UL-MON]   Uplink monitor  period={}ms  RM P2", MONITOR_PERIOD_MS);
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    // Predefined RM command schedule — cycles at each iteration
    // (lab 1 — array, lab 1 — tuples)
    let schedule: [(&str, u8); 6] =
    [
        ("HEARTBEAT",         1),
        ("REQUEST_TELEMETRY", 2),
        ("ADJUST_ANTENNA",    2),
        ("STATUS_QUERY",      1),
        ("THERMAL_CHECK",     1),
        ("ADJUST_GAIN",       3),
    ];

    loop
    {
        // lab 8 — select!
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[UL-MON]   Exit."); return; }
            _ = sleep(Duration::from_millis(MONITOR_PERIOD_MS)) => {}
        }

        let expected_ms = iter * MONITOR_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Drain processed telemetry from backlog  (lab 11 pop pattern)
        {
            let mut bl = backlog.lock().unwrap();
            for _ in 0..5 { if bl.pop_front().is_none() { break; } }
        }

        // Generate next scheduled command
        let (cmd_name, priority) = schedule[(iter as usize) % schedule.len()];
        let cmd = Command
        {
            id:          iter,
            command:     cmd_name.to_string(),
            priority,
            deadline_ms: now_ms() + 2_000,
        };

        // Validate against interlock before queuing  (lab 7 match)
        let il_state = interlock.lock().unwrap().clone();
        match il_state
        {
            InterlockState::Clear =>
            {
                println!(
                    "[UL-MON]   Queuing CMD #{} '{cmd_name}' prio={priority} \
                     drift={drift_ms:+}ms",
                    iter
                );
                cmd_queue.lock().unwrap().push(cmd);
            }
            InterlockState::FaultActive | InterlockState::CriticalAlert =>
            {
                let log = format!(
                    "[{}ms] [BLOCKED] CMD #{} '{cmd_name}' — interlock: {il_state:?}",
                    now_ms(), iter
                );
                println!("{log}");
                let mut m = metrics.lock().unwrap();
                m.cmds_blocked += 1;
                m.cmd_log.push(log);
            }
        }

        metrics.lock().unwrap().task_drift_ms.push(drift_ms);
        iter += 1;
    }
}

// =============================================================================
//  PART 3 — FAULT MANAGEMENT AND INTERLOCKS   (RM P3 — 2000ms)
//
//  Scans the interlock state, times fault duration (lab 3 Instant),
//  clears the interlock after simulated recovery, and escalates to
//  CriticalAlert when recovery exceeds the 100ms assignment threshold.
//  Uses lab 7's consecutive-miss counter pattern.
// =============================================================================

async fn fault_manager_task(
    metrics:   Arc<Mutex<GcsMetrics>>,
    interlock: Arc<Mutex<InterlockState>>,
    shutdown:  CancellationToken,
)
{
    println!("[FAULT-MGR] Fault manager  period={}ms  RM P3", ALERT_PERIOD_MS);
    let mut iter:          u64          = 0;
    let task_start:        Instant      = Instant::now();
    let mut fault_started: Option<Instant> = None;
    let mut missed_count:  u32          = 0; // lab 7 — consecutive miss counter

    loop
    {
        // lab 8 — select!
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[FAULT-MGR] Exit."); return; }
            _ = sleep(Duration::from_millis(ALERT_PERIOD_MS)) => {}
        }

        let expected_ms = iter * ALERT_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        let il_state = interlock.lock().unwrap().clone();

        // lab 7 — match on InterlockState, mirrors run_sensor_loop()
        match il_state
        {
            InterlockState::Clear =>
            {
                // lab 7 — reset counter on success (glitch_count pattern)
                missed_count  = 0;
                fault_started = None;
            }

            InterlockState::FaultActive =>
            {
                missed_count += 1;

                // Start the fault clock if this is the first tick  (lab 3 Instant)
                let t0 = fault_started.get_or_insert_with(Instant::now);
                let elapsed_ms = t0.elapsed().as_millis() as u64;

                metrics.lock().unwrap().interlock_latency_ms.push(elapsed_ms);

                println!(
                    "[FAULT-MGR] Fault active for {}ms  consec={}  drift={drift_ms:+}ms",
                    elapsed_ms, missed_count
                );

                // Attempt recovery after 3 consecutive fault scans  (lab 7 threshold)
                if missed_count >= 3
                {
                    println!("[FAULT-MGR] Attempting recovery...");
                    sleep(Duration::from_millis(60)).await; // simulated subsystem reset

                    let total_ms = fault_started
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);

                    {
                        let mut m = metrics.lock().unwrap();
                        m.fault_recovery_ms.push(total_ms);

                        if total_ms > GROUND_ALERT_LIMIT_MS
                        {
                            // Escalate — assignment says trigger critical ground alert
                            *interlock.lock().unwrap() = InterlockState::CriticalAlert;
                            let alert = format!(
                                "[{}ms] !!! CRITICAL GROUND ALERT !!! \
                                 Recovery {}ms > {}ms limit",
                                now_ms(), total_ms, GROUND_ALERT_LIMIT_MS
                            );
                            println!("{alert}");
                            m.ground_alerts.push(alert);
                        }
                        else
                        {
                            // Recovery succeeded — clear interlock
                            *interlock.lock().unwrap() = InterlockState::Clear;
                            println!(
                                "[FAULT-MGR] Recovery complete in {total_ms}ms — \
                                 interlock cleared."
                            );
                            missed_count  = 0;
                            fault_started = None;
                        }
                    }
                }
            }

            InterlockState::CriticalAlert =>
            {
                // In production this requires operator intervention.
                // For simulation, auto-clear after 5 seconds.
                println!("[FAULT-MGR] CRITICAL ALERT active — awaiting operator clearance...");
                sleep(Duration::from_secs(5)).await;
                *interlock.lock().unwrap() = InterlockState::Clear;
                println!("[FAULT-MGR] Critical alert cleared (simulated clearance).");
                missed_count  = 0;
                fault_started = None;
            }
        }

        {
            let mut m = metrics.lock().unwrap();
            m.task_drift_ms.push(drift_ms);
            if drift_ms.abs() > 20
            {
                m.task_jitter_us.push((drift_ms * 1_000).unsigned_abs() as i64);
            }
        }

        iter += 1;
    }
}

// =============================================================================
//  PART 4 — SYSTEM PERFORMANCE MONITORING   (live report every 10s)
// =============================================================================

async fn metrics_reporter_task(
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => return,
            _ = sleep(Duration::from_secs(10)) => {}
        }
        print_report(&metrics.lock().unwrap());
    }
}

fn print_report(m: &GcsMetrics)
{
    let decode_pct = if m.pkts_received > 0
        { m.pkts_decoded as f64 / m.pkts_received as f64 * 100.0 }
        else { 0.0 };

    let avg_backlog = if m.backlog_samples.is_empty() { 0.0 }
        else
        {
            m.backlog_samples.iter().sum::<usize>() as f64
                / m.backlog_samples.len() as f64
        };

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!(  "║  GCS LIVE REPORT  at {}ms", now_ms());
    println!(  "╠══════════════════════════════════════════════════════╣");
    println!("║  Telemetry pipeline");
    println!("║    Received: {}  Decoded: {} ({:.1}%)  Avg backlog: {:.1}  Uplink restarts: {}",
        m.pkts_received, m.pkts_decoded, decode_pct, avg_backlog, m.uplink_restarts);
    println!("║");
    println!("║  Link Jitter (µs)  — inter-packet arrival variance");
    print_stat_row("Link jitter", &m.link_jitter_us);
    println!("║");
    println!("║  Decode Latency (µs)  deadline={}ms", DECODE_DEADLINE_MS);
    let dl: Vec<i64> = m.decode_latency_us.iter().map(|&v| v as i64).collect();
    print_stat_row("MQTT recv to decode done", &dl);
    println!("║");
    println!("║  Reception Drift (ms)  — actual vs expected interval");
    print_stat_row("Inter-packet drift", &m.reception_drift_ms);
    println!("║");
    println!("║  Task Execution (GCS RM tasks)");
    print_stat_row("Task jitter (µs)", &m.task_jitter_us);
    print_stat_row("Task drift (ms)",  &m.task_drift_ms);
    println!("║");
    println!("║  Command Uplink");
    println!("║    Sent: {}  Blocked: {}  Deadline misses: {}",
        m.cmds_sent, m.cmds_blocked, m.cmds_deadline_miss);
    println!("║");
    println!("║  Fault Management");
    println!("║    Faults from OCS: {}  Ground alerts: {}",
        m.faults_received, m.ground_alerts.len());
    if !m.fault_recovery_ms.is_empty()
    {
        let max_r = m.fault_recovery_ms.iter().max().unwrap();
        let avg_r = m.fault_recovery_ms.iter().sum::<u64>() as f64
                    / m.fault_recovery_ms.len() as f64;
        println!("║  Recovery: max={}ms  avg={avg_r:.1}ms  limit={}ms",
            max_r, GROUND_ALERT_LIMIT_MS);
    }
    if !m.interlock_latency_ms.is_empty()
    {
        let il: Vec<i64> = m.interlock_latency_ms.iter().map(|&v| v as i64).collect();
        print_stat_row("Interlock latency (ms)", &il);
    }
    if !m.ground_alerts.is_empty()
    {
        println!("║");
        println!("║  CRITICAL GROUND ALERTS:");
        for a in &m.ground_alerts { println!("║    {a}"); }
    }
    if !m.cmd_log.is_empty()
    {
        println!("║");
        println!("║  Recent events ({} total):", m.cmd_log.len());
        for e in m.cmd_log.iter().rev().take(4) { println!("║    {e}"); }
    }
    println!("╚══════════════════════════════════════════════════════╝\n");
}

// =============================================================================
//  MAIN   (lab 6 — #[tokio::main] + tokio::spawn)
// =============================================================================

#[tokio::main]
async fn main()
{
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  GCS — Ground Control Station                         ║");
    println!("║  CT087-3-3  |  Student B  |  Soft RTS on Tokio        ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // ── Shared state  (lab 2 — Arc<Mutex<T>>)
    let metrics   = Arc::new(Mutex::new(GcsMetrics::default()));
    let backlog   = Arc::new(Mutex::new(VecDeque::<TelemetryPacket>::new())); // lab 11 job queue
    let cmd_queue = Arc::new(Mutex::new(Vec::<Command>::new()));               // lab 11 job queue
    let interlock = Arc::new(Mutex::new(InterlockState::Clear));

    // ── CancellationToken  (lab 8 — graceful shutdown)
    let shutdown = CancellationToken::new();

    // ── MQTT internal pipeline  (lab 11 — mpsc channel)
    // mqtt_receiver_task produces; telemetry_decoder_task consumes
    let (mqtt_tx, mqtt_rx) = mpsc::channel::<RawMqttMsg>(100);

    // ── Connect MQTT  (lab 9)
    let mqtt_client = setup_mqtt(shutdown.clone()).await;

    println!("[GCS] Config:");
    println!("      MQTT broker      : {MQTT_BROKER}:{MQTT_PORT}");
    println!("      OCS topics       : ocs/{STUDENT_ID}/{{telemetry|status|downlink|alerts}}");
    println!("      Command uplink   : gcs/{STUDENT_ID}/commands");
    println!("      Decode deadline  : {DECODE_DEADLINE_MS}ms");
    println!("      Ground alert at  : {GROUND_ALERT_LIMIT_MS}ms recovery");
    println!("      Sim duration     : {SIM_DURATION_S}s\n");

    // ── Spawn all GCS subsystems  (lab 6 — tokio::spawn)
    // Arc::clone() on every argument — lab 2 pattern

    // Part 1: MQTT receive + telemetry decoder pipeline
    tokio::spawn(mqtt_receiver_task(
        mqtt_tx, Arc::clone(&metrics), shutdown.clone(),
    ));
    tokio::spawn(telemetry_decoder_task(
        mqtt_rx,
        Arc::clone(&backlog),
        Arc::clone(&metrics),
        Arc::clone(&interlock),
        shutdown.clone(),
    ));

    // Part 2: lab 8 supervisor wraps the fragile uplink task
    tokio::spawn(uplink_supervisor_task(
        Arc::clone(&cmd_queue),
        Arc::clone(&metrics),
        Arc::clone(&interlock),
        mqtt_client,
        shutdown.clone(),
    ));

    // Part 2: RM background tasks in priority order
    tokio::spawn(gcs_health_monitor_task(
        Arc::clone(&backlog),
        Arc::clone(&metrics),
        Arc::clone(&interlock),
        shutdown.clone(),
    ));
    tokio::spawn(uplink_monitor_task(
        Arc::clone(&cmd_queue),
        Arc::clone(&backlog),
        Arc::clone(&metrics),
        Arc::clone(&interlock),
        shutdown.clone(),
    ));

    // Part 3: fault manager — RM P3
    tokio::spawn(fault_manager_task(
        Arc::clone(&metrics),
        Arc::clone(&interlock),
        shutdown.clone(),
    ));

    // Part 4: live report every 10s
    tokio::spawn(metrics_reporter_task(
        Arc::clone(&metrics),
        shutdown.clone(),
    ));

    println!("[GCS] All 8 tasks online. Running for {SIM_DURATION_S}s...\n");

    // ── Run for SIM_DURATION_S then shut down  (lab 8 — CancellationToken)
    sleep(Duration::from_secs(SIM_DURATION_S)).await;

    println!("\n[GCS] Time elapsed — requesting graceful shutdown...");
    shutdown.cancel();
    sleep(Duration::from_millis(500)).await;

    println!("\n[GCS] FINAL REPORT:");
    print_report(&metrics.lock().unwrap());
    println!("[GCS] Done.");
}