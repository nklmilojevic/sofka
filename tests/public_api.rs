use std::future::Future;

use kube::core::DynamicObject;
use ratatui::style::{Color, Style};
use sofka::app::App;
use sofka::plugin_catalog::{self, Artifact};
use sofka::plugins::{self, Job};
use sofka::store::{Msg, StatusClaim};
use sofka::theme;
use sofka::timeline::Timeline;

#[test]
fn public_helper_signatures_still_compile() {
    let _: fn(&App, &DynamicObject) -> Option<usize> = App::node_pods_for;
    let _: fn(&App, &DynamicObject) -> String = App::node_pods_cell;
    let _: fn(&mut Timeline, &str, &str, Option<&DynamicObject>, &DynamicObject) =
        Timeline::observe;
    let _: fn(&mut Timeline, &str, &str) = Timeline::observe_delete;
    let _: fn(&DynamicObject, &str) -> Option<String> = sofka::views::condition_status;
    let _: fn() -> Color = theme::flamingo;
    let _: fn() -> Color = theme::surface2;
    let _: fn() -> Color = theme::mantle;
    let _: fn() -> Style = theme::accent;
    let _: fn(StatusClaim) -> Msg = |claim| Msg::FindResults {
        generation: 0,
        claim,
        query: String::new(),
        items: Vec::new(),
        warn: None,
    };

    fn accepts_output_future(_: impl Future<Output = std::io::Result<std::process::Output>>) {}
    accepts_output_future(plugins::execute(Job {
        label: String::new(),
        namespace: String::new(),
        argv: Vec::new(),
        directory: None,
        request: None,
        object: None,
        forward: None,
    }));
}

#[tokio::test]
async fn public_artifact_helpers_keep_the_official_catalog_default() {
    let cache = std::env::temp_dir().join(format!("sofka-public-api-{}", std::process::id()));
    let bytes = b"cached artifact";
    let artifact = Artifact {
        platform: "any".into(),
        url: "https://github.com/nklmilojevic/sofka-plugins/releases/download/v1/test.tar.zst"
            .into(),
        blake3: blake3::hash(bytes).to_hex().to_string(),
        size: bytes.len() as u64,
    };
    let path = cache
        .join("artifacts")
        .join(format!("{}.tar.zst", artifact.blake3));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();

    let cached = plugin_catalog::artifact_in(&cache, &artifact, true)
        .await
        .unwrap();
    assert_eq!(cached.path(), path);
    assert_eq!(std::fs::read(cached.path()).unwrap(), bytes);

    let foreign = Artifact {
        url: "https://example.invalid/test.tar.zst".into(),
        ..artifact
    };
    for error in [
        plugin_catalog::artifact_in(&cache, &foreign, true)
            .await
            .unwrap_err(),
        plugin_catalog::artifact(&foreign, true).await.unwrap_err(),
    ] {
        assert!(
            error.contains("artifact URL must be a release asset"),
            "{error}"
        );
    }
    assert!(path.is_file());
    std::fs::remove_dir_all(cache).unwrap();
}
