use oreslang_format::{format_source, is_formatted};

#[test]
fn uses_two_blank_lines_between_sibling_functions() {
    let src = r#"fnc one() => int {
return 1;
}
fnc two(): int {
return 2;
}



pub routine main() => void {
return;
}
"#;
    let expected = r#"fnc one() -> int {
  return 1;
}


fnc two() -> int {
  return 2;
}


pub routine main() -> void {
  return;
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn class_contract_headers_and_nested_contracts_indent_without_confusion() {
    let src = r#"define class Child extends Parent<int>, Audited implements Named, Serializable as
pub val String name;

pub render() => String {
return self.name;
}


define interface LocalShape
fnc project(String value) -> String;
end

define trait Retryable
fnc retry(): bool;
end
end
"#;
    let expected = r#"define class Child extends Parent<int>, Audited implements Named, Serializable as
  pub val String name;

  pub render() -> String {
    return self.name;
  }

  define interface LocalShape
    fnc project(String value) => String;
  end

  define trait Retryable
    fnc retry() => bool;
  end
end
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn comments_and_strings_do_not_change_block_depth() {
    let src = r#"fnc text() => String {
// } should not dedent
return "{still text}";
}

fnc template() => String {
return `}`;
}
"#;
    let expected = r#"fnc text() -> String {
  // } should not dedent
  return "{still text}";
}


fnc template() -> String {
  return `}`;
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn formats_if_fi_and_actor_bodies() {
    let src = r#"shared actor Cleaner {
pub fnc compact() => void {
if ready; do
actor.gc();
else
return;
fi
}
}
"#;
    let expected = r#"shared actor Cleaner {
  pub fnc compact() -> void {
    if ready; do
      actor.gc();
    else
      return;
    fi
  }
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn formatter_is_idempotent_and_normalizes_crlf() {
    let src = "pub routine main() => void {
  return;  
}
";
    let once = format_source(src).unwrap();
    let twice = format_source(&once).unwrap();
    assert_eq!(once, twice);
    assert_eq!(
        once,
        "pub routine main() -> void {
  return;
}
"
    );
    assert!(is_formatted(&once).unwrap());
}

#[test]
fn rejects_unbalanced_blocks() {
    let err = format_source(
        "fnc nope() -> void {
",
    )
    .unwrap_err();
    assert!(err.message().contains("unterminated"));
}

#[test]
fn local_interfaces_inside_functions_override_outer_callable_context() {
    let src = r#"fnc outer() => void {
define interface Local
fnc run(): int;
end
return;
}
"#;
    let expected = r#"fnc outer() -> void {
  define interface Local
    fnc run() => int;
  end
  return;
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn braced_interfaces_use_signature_arrows() {
    let src = r#"pub interface Shape {
fnc area() -> f64;
}
"#;
    let expected = r#"pub interface Shape {
  fnc area() => f64;
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}

#[test]
fn annotations_stay_attached_to_the_next_function() {
    let src = r#"fnc one() => int {
return 1;
}
@Ret<self>
fnc two() => int {
return 2;
}
"#;
    let expected = r#"fnc one() -> int {
  return 1;
}


@Ret<self>
fnc two() -> int {
  return 2;
}
"#;
    assert_eq!(format_source(src).unwrap(), expected);
}
