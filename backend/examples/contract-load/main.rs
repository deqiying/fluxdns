//! 显式运行的跨平台 DNS 负载驱动，不启动服务、不注入故障，也不改变生产预算。

mod config;
mod exchange;
mod report;

use std::{collections::VecDeque, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use serde_json::json;
use tokio::{task::JoinSet, time::Instant};

use config::Config;
use exchange::{Observation, TargetClient, Worker};
use report::{Counts, Reporter, utc_ms};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("contract-load: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<bool> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "用法：contract-load CONFIG.json [REPORT_ROOT]\n\
             默认报告根目录：_fluxdns/contract-validation\n\
             仅对已授权的测试目标运行；参数无默认负载。Ctrl-C 停止发送并回收请求。\n\
             退出码：0=测量完整且无请求失败/漏发，1=失败/漏发/提前停止，2=配置或驱动错误。"
        );
        return Ok(true);
    }
    if !(1..=2).contains(&args.len()) {
        return Err("需要 CONFIG.json；使用 --help 查看命令".into());
    }
    let path = PathBuf::from(&args[0]);
    if std::fs::metadata(&path)?.len() > 1024 * 1024 {
        return Err("配置不得超过 1MiB".into());
    }
    let input = std::fs::read(path)?;
    let config: Config = serde_json::from_slice(&input).map_err(|error| {
        format!(
            "配置 JSON/schema 无效：line={} column={}",
            error.line(),
            error.column()
        )
    })?;
    let queries = Arc::new(config.validate()?);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let targets = Arc::new(TargetClient::prepare(&config)?);
    let root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("_fluxdns/contract-validation"));
    let mut reporter = Reporter::create(&config, &input, &root)?;
    println!("报告目录：{}", reporter.directory.display());
    let config = Arc::new(config);
    let mut idle: VecDeque<_> = (0..config.concurrency)
        .map(|_| Worker::new(config.targets.len()))
        .collect();
    let mut tasks = JoinSet::new();
    let mut total = Counts::default();
    let mut by_target: Vec<_> = config.targets.iter().map(|_| Counts::default()).collect();
    let started = Instant::now();
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let mut sequence = 0_u64;
    let mut stopped = "completed";
    let mut sampler = tokio::time::interval(Duration::from_millis(config.sample_interval_ms));
    sampler.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result: Result<()> = async {
        'cycles: for cycle in 0..config.cycles {
            for phase in &config.phases {
                let phase_start = Instant::now();
                let phase_end = phase_start + Duration::from_millis(phase.duration_ms);
                let period = Duration::from_nanos(1_000_000_000 / u64::from(phase.qps));
                let mut next_send = phase_start;
                reporter.write(&json!({
                    "event": "phase", "cycle": cycle + 1, "phase": phase.alias,
                    "elapsed_ms": started.elapsed().as_millis(), "qps": phase.qps
                }))?;
                loop {
                    tokio::select! {
                        biased;
                        signal_result = &mut signal => {
                            signal_result?;
                            stopped = "interrupted";
                            break 'cycles;
                        }
                        _ = tokio::time::sleep_until(phase_end) => {
                            // 阶段截止优先于发送；尾部未调度的 tick 同样属于漏发。
                            if next_send < phase_end {
                                let missed = (phase_end - next_send).as_nanos()
                                    .div_ceil(period.as_nanos()) as u64;
                                total.skipped_schedule += missed;
                                sequence = sequence.wrapping_add(missed);
                            }
                            break;
                        },
                        Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                            let (worker, observation) = completed?;
                            observe(&mut total, &mut by_target, &observation);
                            idle.push_back(worker);
                            if total.failed > config.max_errors {
                                stopped = "error_limit";
                                break 'cycles;
                            }
                        }
                        _ = sampler.tick() => {
                            reporter.write(&json!({
                                "event": "sample", "utc_ms": utc_ms()?,
                                "elapsed_ms": started.elapsed().as_millis(),
                                "cycle": cycle + 1, "phase": phase.alias,
                                "in_flight": tasks.len(), "summary": total.snapshot()
                            }))?;
                        }
                        _ = tokio::time::sleep_until(next_send) => {
                            let now = Instant::now();
                            let lag = now.saturating_duration_since(next_send);
                            if lag > Duration::from_millis(config.max_scheduler_lag_ms) {
                                stopped = "scheduler_lag";
                                break 'cycles;
                            }
                            // open-loop 不补发历史 tick；发送侧饱和显式记漏发，不伪报达到设定 QPS。
                            let missed = (lag.as_nanos() / period.as_nanos()) as u64;
                            total.skipped_schedule += missed;
                            sequence = sequence.wrapping_add(missed);
                            next_send += period * (missed as u32 + 1);
                            let target = (sequence % targets.len() as u64) as usize;
                            if let Some(worker) = idle.pop_front() {
                                total.dispatched += 1;
                                by_target[target].dispatched += 1;
                                tasks.spawn(worker.run(config.clone(), targets.clone(), queries.clone(), sequence));
                            } else {
                                total.skipped_capacity += 1;
                                by_target[target].skipped_capacity += 1;
                            }
                            sequence = sequence.wrapping_add(1);
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;

    // 停止新增请求后，只等待已有请求的原 timeout；异常路径同样 abort 并 join。
    let drain = async {
        while let Some(completed) = tasks.join_next().await {
            let (worker, observation) = completed?;
            observe(&mut total, &mut by_target, &observation);
            idle.push_back(worker);
        }
        Ok::<_, tokio::task::JoinError>(())
    };
    let drained = tokio::time::timeout(config.timeout() + Duration::from_secs(1), drain).await;
    let cleanup_ok = matches!(drained, Ok(Ok(())));
    if !cleanup_ok {
        stopped = "driver_cleanup_failed";
    } else if result.is_err() {
        stopped = "driver_error";
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    drop(idle);
    drop(targets);
    let elapsed = started.elapsed();
    let complete = stopped == "completed"
        && total.failed == 0
        && total.skipped_capacity == 0
        && total.skipped_schedule == 0
        && total.dispatched == total.completed
        && total.succeeded > 0;
    reporter.finish(&json!({
        "event": "finished",
        "utc_ms": utc_ms()?,
        "stop_reason": stopped,
        "measurement_complete": complete,
        "acceptance": "measurement_only",
        "cleanup_completed": cleanup_ok,
        "elapsed_ms": elapsed.as_millis(),
        "completed_qps": total.completed as f64 / elapsed.as_secs_f64(),
        "unaccounted": total.dispatched - total.completed,
        "summary": total.snapshot(),
        "targets": config.targets.iter().enumerate().map(|(index, target)| json!({
            "alias": target.alias, "summary": by_target[index].snapshot()
        })).collect::<Vec<_>>()
    }))?;
    result?;
    Ok(complete)
}

fn observe(total: &mut Counts, by_target: &mut [Counts], observation: &Observation) {
    total.observe(observation);
    by_target[observation.target].observe(observation);
}
