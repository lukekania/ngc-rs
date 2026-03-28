use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/simple-app")
}

#[test]
fn test_transform_main() {
    let source = std::fs::read_to_string(fixture_root().join("src/main.ts"))
        .expect("should read fixture");
    let result =
        ngc_ts_transform::transform_source(&source, "main.ts").expect("transform should succeed");
    insta::assert_snapshot!("main_js", result);
}

#[test]
fn test_transform_app_component() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/app.component.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "app.component.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("app_component_js", result);
}

#[test]
fn test_transform_app_config() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/app.config.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "app.config.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("app_config_js", result);
}

#[test]
fn test_transform_app_routes() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/app.routes.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "app.routes.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("app_routes_js", result);
}

#[test]
fn test_transform_shared_index() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/shared/index.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "index.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("shared_index_js", result);
}

#[test]
fn test_transform_logger() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/shared/logger.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "logger.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("logger_js", result);
}

#[test]
fn test_transform_utils() {
    let source = std::fs::read_to_string(fixture_root().join("src/app/shared/utils.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "utils.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("utils_js", result);
}

#[test]
fn test_transform_environment() {
    let source = std::fs::read_to_string(fixture_root().join("src/environments/environment.ts"))
        .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "environment.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("environment_js", result);
}

#[test]
fn test_transform_environment_prod() {
    let source =
        std::fs::read_to_string(fixture_root().join("src/environments/environment.prod.ts"))
            .expect("should read fixture");
    let result = ngc_ts_transform::transform_source(&source, "environment.prod.ts")
        .expect("transform should succeed");
    insta::assert_snapshot!("environment_prod_js", result);
}

#[test]
fn test_transform_all_fixtures() {
    let fixture = fixture_root().join("tsconfig.app.json");
    let graph =
        ngc_project_resolver::resolve_project(&fixture).expect("fixture should resolve");

    for file_path in graph.graph.node_weights() {
        let source = std::fs::read_to_string(file_path).expect("should read fixture file");
        let file_name = file_path.file_name().unwrap().to_string_lossy();
        ngc_ts_transform::transform_source(&source, &file_name)
            .unwrap_or_else(|_| panic!("should transform {file_name}"));
    }
}
