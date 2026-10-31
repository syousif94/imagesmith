//! Comment-preserving recipe edits on the toml_edit document.

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

pub fn table_mut<'a>(doc: &'a mut DocumentMut, section: &str) -> &'a mut Table {
    let item = doc.as_table_mut().entry(section).or_insert(toml_edit::table());
    if !item.is_table() {
        *item = toml_edit::table();
    }
    item.as_table_mut().unwrap()
}

/// Sets `section.key`; an empty string removes the key.
pub fn set_str(doc: &mut DocumentMut, section: &str, key: &str, v: &str) {
    let t = table_mut(doc, section);
    if v.is_empty() {
        t.remove(key);
    } else {
        set_keep_decor(t, key, Value::from(v));
    }
}

pub fn set_value(doc: &mut DocumentMut, section: &str, key: &str, v: impl Into<Value>) {
    set_keep_decor(table_mut(doc, section), key, v.into());
}

/// Replaces a value while keeping its trailing comment.
fn set_keep_decor(t: &mut Table, key: &str, mut v: Value) {
    if let Some(old) = t.get(key).and_then(Item::as_value) {
        let d = old.decor().clone();
        *v.decor_mut() = d;
    }
    t.insert(key, Item::Value(v));
}

fn array_mut<'a>(t: &'a mut Table, key: &str) -> &'a mut Array {
    let item = t.entry(key).or_insert(Item::Value(Value::Array(Array::new())));
    if !item.is_array() {
        *item = Item::Value(Value::Array(Array::new()));
    }
    item.as_array_mut().unwrap()
}

pub fn push_str(doc: &mut DocumentMut, section: &str, key: &str, v: &str) {
    let a = array_mut(table_mut(doc, section), key);
    if !a.iter().any(|x| x.as_str() == Some(v)) {
        a.push(v);
    }
}

pub fn remove_at(doc: &mut DocumentMut, section: &str, key: &str, i: usize) {
    let a = array_mut(table_mut(doc, section), key);
    if i < a.len() {
        a.remove(i);
    }
}

/// Removes `v` from `section.key`; false if it wasn't there.
pub fn remove_str(doc: &mut DocumentMut, section: &str, key: &str, v: &str) -> bool {
    let a = array_mut(table_mut(doc, section), key);
    let pos = a.iter().position(|x| x.as_str() == Some(v));
    match pos {
        Some(i) => {
            a.remove(i);
            true
        }
        None => false,
    }
}

/// Converts JSON (from the assistant) to a TOML value; null has no TOML equivalent.
pub fn json_to_value(j: &serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match j {
        J::Null => return Err("null is not a TOML value".into()),
        J::Bool(b) => Value::from(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::from(i),
            None => Value::from(n.as_f64().ok_or("number out of range")?),
        },
        J::String(s) => multiline(s),
        J::Array(items) => {
            let mut a = Array::new();
            for i in items {
                a.push(json_to_value(i)?);
            }
            Value::Array(a)
        }
        J::Object(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in m {
                t.insert(k, json_to_value(v)?);
            }
            Value::InlineTable(t)
        }
    })
}

pub fn aot_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut ArrayOfTables {
    let item = doc.as_table_mut().entry(key).or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        *item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    item.as_array_of_tables_mut().unwrap()
}

pub fn aot_entry<'a>(doc: &'a mut DocumentMut, key: &str, i: usize) -> Option<&'a mut Table> {
    aot_mut(doc, key).get_mut(i)
}

pub fn aot_remove(doc: &mut DocumentMut, key: &str, i: usize) {
    let a = aot_mut(doc, key);
    if i < a.len() {
        a.remove(i);
    }
    if a.is_empty() {
        doc.as_table_mut().remove(key);
    }
}

pub fn aot_swap(doc: &mut DocumentMut, key: &str, i: usize, j: usize) {
    let a = aot_mut(doc, key);
    if i >= a.len() || j >= a.len() {
        return;
    }
    let mut tables: Vec<Table> = a.iter().cloned().collect();
    tables.swap(i, j);
    let mut out = ArrayOfTables::new();
    for t in tables {
        out.push(t);
    }
    *a = out;
}

pub fn aot_push(doc: &mut DocumentMut, key: &str, t: Table) {
    aot_mut(doc, key).push(t);
}

pub fn table_from(pairs: &[(&str, Value)]) -> Table {
    let mut t = Table::new();
    for (k, v) in pairs {
        t.insert(k, Item::Value(v.clone()));
    }
    t
}

pub fn str_array(items: &[&str]) -> Value {
    let mut a = Array::new();
    for i in items {
        a.push(*i);
    }
    Value::Array(a)
}

/// A string value written as a TOML multi-line literal when possible.
pub fn multiline(s: &str) -> Value {
    if s.contains('\n') && !s.contains("'''") && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        if let Ok(v) = format!("'''\n{s}'''").parse::<Value>() {
            return v;
        }
    }
    Value::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_comments() {
        let mut doc: DocumentMut = "[system]\nhostname = \"a\" # keep\n\n[packages]\ninstall = [\"x\"]\n".parse().unwrap();
        set_str(&mut doc, "system", "hostname", "b");
        push_str(&mut doc, "packages", "install", "y");
        push_str(&mut doc, "packages", "install", "y");
        aot_push(&mut doc, "steps", table_from(&[("name", "s1".into()), ("run", "true".into())]));
        aot_push(&mut doc, "steps", table_from(&[("name", "s2".into()), ("run", "true".into())]));
        aot_swap(&mut doc, "steps", 0, 1);
        let s = doc.to_string();
        assert!(s.contains("hostname = \"b\" # keep"), "{s}");
        assert!(s.contains("install = [\"x\", \"y\"]"), "{s}");
        assert!(s.find("s2").unwrap() < s.find("s1").unwrap());
        aot_remove(&mut doc, "steps", 0);
        aot_remove(&mut doc, "steps", 0);
        assert!(!doc.to_string().contains("[[steps]]"));

        aot_push(&mut doc, "steps", table_from(&[("run", multiline("apt-get update\necho 'x'\n"))]));
        let s = doc.to_string();
        assert!(s.contains("run = '''\napt-get update\necho 'x'\n'''"), "{s}");
        let r: toml_edit::DocumentMut = s.parse().unwrap();
        assert_eq!(r["steps"][0]["run"].as_str(), Some("apt-get update\necho 'x'\n"));
        assert_eq!(multiline("one line").as_str(), Some("one line"));
    }
}
