use markdoc::{parse, evaluate_conditionals, types::{Context, Scalar}};
use std::collections::HashMap;

fn main() {
    let src = r#"{% if equals(, ) %}SHELF_ONLY{% /if %}
{% if equals(, ) %}HOOK_ONLY{% /if %}
ALWAYS"#;
    let doc = parse(src, None).unwrap();
    let mut vars = HashMap::new();
    vars.insert(\"model\".into(), Scalar::String(\"MiR250 Base Robot\".into()));
    vars.insert(\"mir250_shelf_carrier\".into(), Scalar::String(\"MiR250 Shelf Carrier\".into()));
    vars.insert(\"mir250_hook\".into(), Scalar::String(\"MiR250 Hook\".into()));
    let ctx = Context { variables: vars };
    let result = evaluate_conditionals(&doc, &ctx).unwrap();
    println!(\"{result:#?}\");
}
