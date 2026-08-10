# markdoc

Rust implementation of the [Markdoc](https://markdoc.dev) document
language — CommonMark plus a `{% tag %}` extension, with frontmatter,
partials, conditionals, cross-references, and a configurable schema.
Originally a port of Stripe's [JavaScript implementation](https://github.com/markdoc/markdoc).

This crate is **application-agnostic**: it parses, transforms, and
renders to HTML, but knows nothing about how its output is used. The
typed Flux frontmatter view, the PDF renderer, and the Leptos
integration each live in their own sibling crate so consumers don't
pull in dependencies they don't need.

```
.mdoc source
     │
     ▼
   parse  →  Node (AST + frontmatter Scalar)
     │
     ▼
  expand_partials   (recursive, cycle-detected)
  resolve_crossrefs
  evaluate_conditionals
  transform / transform_with_context
     │
     ▼
RenderableTreeNode
     │
     ├──►  renderers::html::render                         ── built in
     ├──►  flux-types::FluxFrontmatter::from_node          ── sibling
     ├──►  markdoc-pdf                                     ── sibling
     └──►  leptos-markdoc <Markdoc/>                       ── sibling
```

## What it does

- **Parses** CommonMark via [`pulldown-cmark`](https://crates.io/crates/pulldown-cmark).
- **Parses Markdoc tags** — `{% tag attr="value" /%}` / `{% tag %}…{% /tag %}`,
  primary-attribute shorthand, self-closing, nested.
- **Frontmatter** — YAML between `---` fences, exposed as a
  `Scalar::Object` on the document node.
- **Partials** — `{% partial file="…" /%}` expanded by a
  `PartialResolver`. Two ready-made resolvers: `FsPartialResolver`
  and `InMemoryPartialResolver`. Cycles are detected and reported.
- **Cross-references** — `{% tag id="…" /%}` declares an anchor;
  `{% tagref id="…" /%}` references one. `resolve_crossrefs` walks
  the tree and binds refs to their targets.
- **Conditionals** — `{% if $expr %}…{% else $other /%}…{% else /%}…{% /if %}`
  evaluated against a `Context` of named variables and functions.
- **Schema validation** — pass a `Config` with tag definitions; the
  validator and transformer enforce attribute types, required
  attributes, allowed children.
- **Rendering** — `renderers::html` ships in-crate for the
  CommonMark + tag baseline. PDF and Leptos renderers live in
  separate sibling crates.

## Usage

The shortest possible roundtrip — markdown in, HTML out:

```rust
use markdoc::{parse, transform, renderers::html, types::Config};

let src = "# Hello\n\nThis is **Markdoc**.";
let doc = parse(src, None)?;
let tree = transform(&doc, &Config::default())?;
let html = html::render(&tree);
```

The full Adeptus-style pipeline, with partials and conditionals:

```rust
use markdoc::{
    parse, transform_with_context, evaluate_conditionals,
    resolve_crossrefs,
    partials::{expand_partials, FsPartialResolver},
    types::Config, Context,
};

let doc = parse(&source, None)?;
let doc = expand_partials(&doc, &FsPartialResolver::new("."))?;
let doc = resolve_crossrefs(&doc);
let ctx = Context::new();
let doc = evaluate_conditionals(&doc, &ctx)?;
let tree = transform_with_context(&doc, &Config::default(), &ctx)?;
```

For ergonomics, a `Markdoc` struct bundles a config and exposes the
same three calls:

```rust
use markdoc::{Markdoc, types::Config};

let md = Markdoc::new(Config::default());
let doc = md.parse(source)?;
let tree = md.transform(&doc)?;
let errors = md.validate(&doc);  // Vec<ValidationError>
```

## Module map

| Module | Purpose |
|--------|---------|
| `parser` | CommonMark + tag tokeniser → `ast::Node` |
| `tag_parser`, `tokenizer` | Lower-level pieces the parser uses |
| `ast` | `Node`, `Function`, `Variable` |
| `frontmatter` | YAML frontmatter extraction |
| `partials` | `expand_partials`, `PartialResolver`, FS + in-memory impls |
| `crossrefs` | `resolve_crossrefs`, `collect_anchors`, `AnchorInfo` |
| `conditionals` | `evaluate_conditionals`, expression evaluation |
| `expression` | The `{% if … %}` expression grammar |
| `functions` | Function-call evaluation in expressions |
| `transformer` | `transform`, `transform_with_context` |
| `validator` | Schema validation against a `Config` |
| `schema`, `tags`, `types` | `Config`, `Schema`, `Scalar`, `RenderableTreeNode`, … |
| `renderers::html` | The built-in HTML renderer |

Top-level re-exports keep the common surface short:
`parse`, `transform`, `transform_with_context`, `evaluate_conditionals`,
`resolve_crossrefs`, `collect_anchors`, `expand_partials`, `validate`,
`Markdoc`, `Context`, `Node`, plus `Function`, `Variable`, and the
two partial resolvers.

## Sibling crates

| Crate | Builds on this for… |
|-------|---------------------|
| [`flux-types`](../flux-types) | Typed view of the YAML frontmatter for Adeptus documents |
| [`markdoc-pdf`](../markdoc-pdf) | CLI + library that renders a parsed tree to PDF (krilla + parley) |
| [`leptos-markdoc`](../leptos-markdoc) | Leptos components for web/mobile UI rendering |

Each sibling depends on this crate via a relative `path = "../markdoc"`
reference and pins its own additional deps. They were previously a
single workspace; splitting them lets each evolve and version
independently.

## Differences from the JavaScript original

- **Tokeniser**: `pulldown-cmark` instead of `markdown-it`.
- **Type system**: Rust enums (`Scalar`, `Node`, `RenderableTreeNode`)
  rather than untyped JS objects.
- **Errors**: `Result<T, …>` rather than thrown exceptions.
- **HTML output**: deliberately close to Stripe's reference, but not
  byte-identical.

## Examples

A standalone runnable example lives in [`examples/basic.rs`](examples/basic.rs):

```sh
cargo run --example basic
```

`examples/example.mdoc` shows a full Markdoc source with frontmatter,
variable interpolation, and tags. The matching expected HTML output
is in `examples/output-mdoc.html`.

## License

MIT. Original Markdoc © Stripe, used here under the same licence.

## MiR Modifications

* Support for light indicator table and image grids
* Support for various image sizes
* Implemetned following page-break rules: 
  * Page-breaks cannot occur right after a heading
  * Page-breaks cannot occur just before a list starts (Page-breaks can be in the middle of a list)
  * Page-breaks cannot occur in the middle of a list item, unless the list item is longer than a page.
  * There must be at least three lines of text, a complete paragraph, an image, a notice block, or a table before a page-break.
* Support for column size control
* Bold and italics fix
* Condition and varaible support.