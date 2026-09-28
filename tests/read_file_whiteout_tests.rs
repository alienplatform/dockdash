use dockdash::{Image, Layer};

async fn layer(entries: &[(&str, &[u8])]) -> Layer {
    let mut builder = Layer::builder().unwrap();
    for (path, content) in entries {
        builder = builder.data(path, content, None).unwrap();
    }
    builder.build().await.unwrap()
}

async fn image(layers: Vec<Layer>) -> Image {
    let mut builder = Image::builder();
    for l in layers {
        builder = builder.layer(l);
    }
    builder.build().await.unwrap().0
}

#[tokio::test]
async fn an_ancestor_replaced_by_a_file_hides_the_lower_file() {
    let img = image(vec![
        layer(&[("etc/passwd", b"base")]).await,
        layer(&[("etc", b"now a file")]).await,
    ])
    .await;
    assert_eq!(img.read_file("/etc/passwd").await.unwrap(), None);
}

#[tokio::test]
async fn a_whiteout_for_a_name_prefix_does_not_hide_the_file() {
    let img = image(vec![
        layer(&[("etc/passwd", b"base")]).await,
        layer(&[("etc/.wh.pass", b""), ("etcx/.wh..wh..opq", b"")]).await,
    ])
    .await;
    assert_eq!(
        img.read_file("/etc/passwd").await.unwrap().unwrap(),
        b"base"
    );
}

#[tokio::test]
async fn a_parent_whited_out_and_recreated_in_one_layer_keeps_only_the_new_entries() {
    let img = image(vec![
        layer(&[("etc/passwd", b"base"), ("etc/group", b"g")]).await,
        layer(&[(".wh.etc", b""), ("etc/passwd", b"new")]).await,
    ])
    .await;
    assert_eq!(img.read_file("/etc/passwd").await.unwrap().unwrap(), b"new");
    assert_eq!(img.read_file("/etc/group").await.unwrap(), None);
}

#[tokio::test]
async fn a_root_opaque_marker_hides_every_lower_file() {
    let img = image(vec![
        layer(&[("etc/passwd", b"base")]).await,
        layer(&[(".wh..wh..opq", b""), ("x", b"x")]).await,
    ])
    .await;
    assert_eq!(img.read_file("/etc/passwd").await.unwrap(), None);
    assert!(img.read_file("/").await.is_err());
}
