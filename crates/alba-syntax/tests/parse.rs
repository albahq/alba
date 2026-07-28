//! Snapshot test parsing an example Beamfile that exercises every piece
//! of the DSL's grammar at once.

use alba_syntax::parse;

const EXAMPLE: &str = r#"version "1"

import "api/Beamfile" as api

let profile = env("PROFILE", "debug")
let release = profile == "release"

default build

beam build {
  description "Compile the workspace"
  needs [api:build, codegen]
  inputs ["src/**/*.rs", "Cargo.toml"]
  outputs ["target/{profile}/app"]
  run "cargo build {if release then '--release' else ''}"
}

beam test {
  needs [build]
  run "cargo test"
}

beam lint {
  allow_failure true
  run "cargo clippy -- -D warnings"
}

beam deploy(target) {
  description "Deploy to an environment"
  needs [test]
  executor docker { image "deployer:latest" }
  env { DEPLOY_TARGET = target }
  run "./scripts/deploy.sh {target}"
}
"#;

#[test]
fn parses_an_example_using_every_construct() {
    insta::assert_debug_snapshot!(parse(EXAMPLE).unwrap());
}
