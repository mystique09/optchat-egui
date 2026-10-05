use optchat::attachments;

#[tokio::test]
async fn sends_real_images_and_log_text_and_keeps_original_files() {
    let source = tempfile::tempdir().unwrap();
    let chat = tempfile::tempdir().unwrap();
    let image = source.path().join("screenshot.png");
    image::RgbImage::new(2, 2).save(&image).unwrap();
    let log = source.path().join("app.log");
    std::fs::write(&log, "ERROR: retry failed\ncontext: café").unwrap();
    let message = attachments::prepare("Analyze".into(), &[image.clone(), log], chat.path())
        .await
        .unwrap();
    assert!(message.text.contains("ERROR: retry failed"));
    assert!(!message.text.contains("base64"));
    let block = message
        .blocks
        .iter()
        .find(|b| b["type"] == "image")
        .unwrap();
    assert_eq!(block["source"]["media_type"], "image/png");
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(block["source"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, std::fs::read(image).unwrap());
    let files: Vec<_> = std::fs::read_dir(chat.path().join("attachments"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 2);
    assert!(
        files
            .into_iter()
            .any(|f| std::fs::read(f.unwrap().path()).unwrap() == bytes)
    );
}

#[tokio::test]
async fn invalid_missing_and_oversized_files_do_not_send_partial_messages() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("binary");
    std::fs::write(&file, [0xff, 0x00]).unwrap();
    assert!(
        attachments::prepare("test".into(), std::slice::from_ref(&file), dir.path())
            .await
            .is_err()
    );
    std::fs::write(&file, vec![b'x'; 256 * 1024 + 1]).unwrap();
    assert!(
        attachments::prepare("test".into(), &[file], dir.path())
            .await
            .is_err()
    );
    assert!(
        attachments::prepare("test".into(), &[dir.path().join("missing")], dir.path())
            .await
            .is_err()
    );
    assert!(!dir.path().join("attachments").exists());
}
