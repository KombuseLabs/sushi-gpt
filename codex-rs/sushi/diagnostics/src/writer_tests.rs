use super::super::Decision;
use super::super::Reason;
use super::super::Source;
use super::*;
use pretty_assertions::assert_eq;

fn record() -> Record {
    Record::RoutingDecision(Decision {
        decision_id: Uuid::new_v4(),
        parent_thread_id: codex_protocol::ThreadId::new(),
        parent_turn_id: Some("turn".into()),
        call_id: None,
        child_thread_id: None,
        requested_model: None,
        policy_model: None,
        selected_model: Some("model".into()),
        source: Source::Native,
        reason_code: Reason::Native,
    })
}

#[test]
fn sushi_telemetry_rotates_with_generation_and_sequence() -> anyhow::Result<()> {
    let file = tempfile::tempfile()?;
    let mut reader = file.try_clone()?;
    let mut writer = Writer {
        file,
        process: Uuid::new_v4(),
        generation: Uuid::new_v4(),
        sequence: 0,
        bytes: 0,
    };
    writer.write(record(), /*dropped*/ 0, /*limit*/ 4096)?;
    let generation = writer.generation;
    writer.write(record(), /*dropped*/ 2, /*limit*/ 4096)?;
    assert_ne!(writer.generation, generation);
    reader.rewind()?;
    let parsed: serde_json::Value = serde_json::from_reader(reader)?;
    assert_eq!(
        (
            parsed["sequence"].as_u64(),
            parsed["droppedRecords"].as_u64()
        ),
        (Some(2), Some(2))
    );
    assert_eq!(parsed["generationId"], serde_json::json!(writer.generation));
    assert!(writer.bytes <= 4096);
    Ok(())
}

#[test]
fn sushi_telemetry_queue_never_blocks_and_counts_loss() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let emitter = Emitter {
        sender,
        dropped: Arc::new(AtomicU64::new(0)),
    };
    emitter.emit(record());
    emitter.emit(record());
    assert_eq!(emitter.dropped.load(Ordering::Relaxed), 1);
    drop(receiver);
    emitter.emit(record());
    assert_eq!(emitter.dropped.load(Ordering::Relaxed), 2);
}

#[test]
fn sushi_telemetry_has_one_writer_and_releases_lock() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (emitter, worker) = open(home.path())?;
    assert!(open(home.path()).is_err());
    emitter.emit(record());
    drop(emitter);
    worker.join().expect("writer finishes");
    let (emitter, worker) = open(home.path())?;
    drop(emitter);
    worker.join().expect("writer finishes");
    Ok(())
}

#[cfg(unix)]
#[test]
fn sushi_telemetry_rejects_symlinks_and_hardlinks() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let victim = home.path().join("untouched");
    std::fs::write(&victim, "untouched")?;
    let destination = home.path().join("sushigpt-telemetry.jsonl");
    std::os::unix::fs::symlink(&victim, &destination)?;
    assert!(open(home.path()).is_err());
    std::fs::remove_file(&destination)?;
    std::fs::hard_link(&victim, &destination)?;
    assert!(open(home.path()).is_err());
    assert_eq!(std::fs::read_to_string(victim)?, "untouched");
    Ok(())
}
