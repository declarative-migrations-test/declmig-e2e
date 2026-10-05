# oreslang-format

The canonical formatter for Oreslang source code.

There is deliberately **one format and no style configuration**. The same Rust
library powers the CLI, so editor integrations, CI, and local development use
identical behavior.

## Canonical style

- two spaces per indentation level;
- LF line endings, no trailing whitespace, one final newline;
- at most one ordinary blank line;
- **two blank lines between sibling executable function/routine/method declarations**;
- executable declarations and method implementations use the slim arrow `->`;
- interface/trait callable signatures use the type-level fat arrow `=>`;
- class headers keep `as` after the complete inheritance/conformance clause:

```ores
define class User extends Entity implements Named, Serializable as
  pub val String name;

  pub render() -> String {
    return self.name;
  }
end
```

The nesting engine understands `module`, `class`, `interface`, `trait`,
`struct`, actor/braced bodies, `end`, `if`/`fi`, and `do`/`done`. In particular,
`implements Foo, Bar` never creates formatter nesting; the class body begins
only after the class header and is closed by its matching `end`.

The formatter is intentionally conservative about grammar that is still
changing: it does not reorder declarations, imports, traits, interfaces, or
class conformance lists.

## CLI

```bash
cargo install --path .

# rewrite files in place
oresfmt src examples

# CI / pre-commit mode; exits 1 if anything would change
oresfmt --check .

# stdin -> stdout
oresfmt - < input.ores

# one file -> stdout
oresfmt --stdout example.ores
```

The installed binary is `oresfmt`; `oreslang-format` is also provided as an
alias. Directories are walked recursively and only `.ores` files are selected.

## Rust SDK

```rust
use oreslang_format::{format_source, is_formatted};

let formatted = format_source(source)?;
let clean = is_formatted(&formatted)?;
assert!(clean);
```

`format_source` is idempotent: formatting canonical output again produces the
same bytes.

## Why no configuration?

Oreslang should have one mechanically enforceable source style. This avoids
project-specific formatter drift and gives compiler diagnostics, generated
code, examples, editor integrations, and code review the same layout contract.
