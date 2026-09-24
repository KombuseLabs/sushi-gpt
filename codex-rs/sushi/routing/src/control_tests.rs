use super::Mode;
use super::read;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn agent_model_routing_control_reloads_and_rejects_invalid_input() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("agent-model-routing.mode");
    assert_eq!(read(home.path()).await?, Mode::Configured);
    for (text, mode) in [
        ("off\n", Mode::Off),
        ("rules-only\n", Mode::RulesOnly),
        ("configured\n", Mode::Configured),
    ] {
        tokio::fs::write(&path, text).await?;
        assert_eq!(read(home.path()).await?, mode);
    }
    for bytes in [Vec::new(), b"on".to_vec(), vec![b'x'; 65], vec![255]] {
        tokio::fs::write(&path, bytes).await?;
        assert_eq!(
            read(home.path()).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
    tokio::fs::remove_file(&path).await?;
    tokio::fs::create_dir(&path).await?;
    assert_eq!(
        read(home.path()).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    Ok(())
}
