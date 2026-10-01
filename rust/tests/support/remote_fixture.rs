#[allow(dead_code)]
pub async fn run_auth_fixture(dir: &Path) -> Result<()> {
    let prompts = std::env::temp_dir().join(format!("wch-fixture-{}.prompts", std::process::id()));
    let _ = std::fs::remove_file(&prompts);
    let prompt_count = || {
        std::fs::read_to_string(&prompts)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let local = dir.join("fixture.png");
    std::fs::write(
        &local,
        (0..262144).map(|n| (n % 256) as u8).collect::<Vec<_>>(),
    )?;
    let wsl_dir = String::from_utf8(
        Command::new("wsl.exe")
            .args(["-e", "wslpath", "-u"])
            .arg(dir)
            .output()
            .await?
            .stdout,
    )?
    .trim()
    .to_string();
    let context = String::from_utf8(
        Command::new("wsl.exe")
            .args([
                "-e",
                "sh",
                "-c",
                "printf '%s\\n' \"$WSL_DISTRO_NAME\"; id -un",
            ])
            .output()
            .await?
            .stdout,
    )?;
    let mut context_lines = context.lines();
    let distro = context_lines.next().unwrap().to_string();
    let user = context_lines.next().unwrap().to_string();
    // Exercise actual /proc discovery, not just synthetic parser input.
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let socket = std::fs::read_to_string(dir.join("agent-socket"))?;
        let config = format!("{wsl_dir}/wsl.conf");
        let mut cmd = Command::new("wsl.exe");
        cmd.args(["-e", "sh", "-c", r#"cd "$1" && export SSH_AUTH_SOCK="$2" WT_SESSION=wch-fixture && exec ssh -o BatchMode=yes -F "$3" key"#, "sh", &wsl_dir, socket.trim(), &config])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd.spawn()?;
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"printf 'DISCOVERY_READY\\n'\n").await?;
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line)).await??;
        assert_eq!(line.trim(), "DISCOVERY_READY");
        let discovered = discover_wsl();
        let found = discovered
            .iter()
            .find(|s| s.args.contains(&config) && s.destination == "key")
            .expect("WSL fixture session must be discovered");
        assert_eq!(found.cwd.as_deref(), Some(wsl_dir.as_str()));
        assert_eq!(found.auth_sock.as_deref(), Some(socket.trim()));
        assert_eq!(found.wsl_context, Some((distro.clone(), user.clone())));
        input.write_all(b"exit\n").await?;
        child.wait().await?;
        println!("PASS Wsl discovery: original argv, cwd, user/distro and agent socket");
    }
    for backend in [SshBackend::Windows, SshBackend::Wsl] {
        let mut cases = vec![
            ("password", 1),
            ("key", 0),
            ("encrypted", 1),
            ("cancel", 1),
            ("wrong", 1),
            ("changed", 0),
        ];
        if backend == SshBackend::Wsl {
            cases.extend([("agent", 0), ("reused", 0)]);
        }
        for (case, expected_prompts) in cases {
            let windows = backend == SshBackend::Windows;
            let config = if windows {
                dir.join("windows.conf").to_string_lossy().to_string()
            } else {
                format!("{wsl_dir}/wsl.conf")
            };
            let program = if windows {
                PathBuf::from("C:\\Windows\\System32\\OpenSSH\\ssh.exe")
            } else {
                "/usr/bin/ssh".into()
            };
            let mut session =
                session_from_argv(backend, 1, program, &["-F".into(), config, case.into()], 0)
                    .unwrap();
            if !windows {
                session.wsl_context = Some((distro.clone(), user.clone()));
                session.cwd = Some(wsl_dir.clone());
            }
            if case == "agent" {
                session.auth_sock = Some(
                    std::fs::read_to_string(dir.join("agent-socket"))?
                        .trim()
                        .to_string(),
                );
            }
            let before = prompt_count();
            let uploader = RemoteUploader::new();
            let result = uploader.upload(&session, &local, true).await;
            if case == "cancel" || case == "wrong" || case == "changed" {
                assert!(
                    result.is_err(),
                    "{backend:?} {case} unexpectedly authenticated"
                );
            } else {
                let path = result?;
                session.pid = 2;
                assert_eq!(uploader.upload(&session, &local, true).await?, path);
                let local2 = dir.join("second.png");
                std::fs::write(&local2, b"second file")?;
                uploader.upload(&session, &local2, true).await?;
                assert_eq!(uploader.connections.lock().await.len(), 1);
            }
            assert_eq!(
                prompt_count() - before,
                expected_prompts,
                "{backend:?} {case} askpass count"
            );
            uploader.cleanup_on_exit().await;
            assert_eq!(
                prompt_count() - before,
                expected_prompts,
                "exit must not prompt"
            );
            println!("PASS {backend:?} {case}: upload/cache/new-tab/reuse/exit (or cancellation), {expected_prompts} prompts");
            if case == "password" {
                // Reconnect after a channel has actually exited, without using its stale cache.
                let uploader = RemoteUploader::new();
                let old = uploader.upload(&session, &local, true).await?;
                uploader.connections.lock().await[0].transport.close().await;
                let new = uploader.upload(&session, &local, true).await?;
                assert_ne!(old, new);
                uploader.cleanup_on_exit().await;
                assert_eq!(prompt_count() - before, expected_prompts + 2);
                println!("PASS {backend:?} reconnect: discarded old cache and authenticated a fresh channel");
            }
        }
    }
    let _ = std::fs::remove_file(prompts);
    Ok(())
}
