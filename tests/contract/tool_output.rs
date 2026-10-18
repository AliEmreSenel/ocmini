use super::*;

#[tokio::test]
async fn large_shell_and_web_results_save_full_output_and_replay_only_the_preview() {
    let text = format!(
        "BEGIN\n{}MIDDLE_SENTINEL\n{}END\n",
        "ordinary line\n".repeat(20_000),
        "ordinary line\n".repeat(20_000)
    );
    let served = text.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/large", listener.local_addr().unwrap());
    let router = Router::new().route(
        "/large",
        axum::routing::get(move || {
            let text = served.clone();
            async move { text }
        }),
    );
    let web = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    for (name, input) in [
        ("bash", json!({"command":"cat input.txt"})),
        ("webfetch", json!({"url":url,"format":"text"})),
    ] {
        let mut provider = Provider::start(vec![
            tool_script(name, input, "large-result"),
            text_script("Saved", ""),
            text_script("Continued", ""),
        ])
        .await;
        let mut server = TestServer::start(&provider, json!({})).await;
        std::fs::write(server.directory.join("input.txt"), &text).unwrap();
        let id = server.session(allow()).await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_large", "Run the tool"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        provider.request().await;
        let request = provider.request().await;
        let sent = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(sent.starts_with("BEGIN"));
        assert!(sent.contains("END"));
        assert!(sent.contains("Full tool output saved"));
        assert!(!sent.contains("MIDDLE_SENTINEL"));
        assert!(sent.len() < 52 * 1024);
        let messages = server.messages(&id).await;
        let tool = messages
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .find(|part| part["type"] == "tool")
            .unwrap();
        assert_eq!(tool["state"]["metadata"]["truncated"], true);
        assert_eq!(tool["state"]["output"], sent);
        let path = PathBuf::from(tool["state"]["metadata"]["outputPath"].as_str().unwrap());
        assert!(path.starts_with(server.root.path().join(".local/share/ocmini/tool-output")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        server.restart().await;
        server
            .post(
                &format!("/session/{id}/message"),
                &prompt("msg_after_restart", "Continue"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let request = provider.request().await;
        let sent = request.to_string();
        assert!(sent.contains(&path.to_string_lossy().into_owned()));
        assert!(!sent.contains("MIDDLE_SENTINEL"));
        assert!(sent.len() < 65 * 1024);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
    web.abort();
}
