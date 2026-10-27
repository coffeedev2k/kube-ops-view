//! Structural JSON diff in the JSON-delta stanza format.
//!
//! The UI applies the diff with `JSON_delta.patch` (`app/src/vendor/json_delta.js`).
//! A diff is a list of stanzas:
//! - `[keypath, value]`: set the value at keypath (add or replace)
//! - `[keypath]`: delete the value at keypath (array elements are spliced out)
//!
//! Arrays are compared index by index (same as `json_delta.diff(array_align=False)`).
//! Array deletes come in descending index order, so each splice keeps the next index valid.

use serde_json::{Value, json};

pub fn diff(left: &Value, right: &Value) -> Vec<Value> {
    let mut stanzas = Vec::new();
    let mut keypath = Vec::new();
    diff_into(left, right, &mut keypath, &mut stanzas);
    stanzas
}

fn diff_into(left: &Value, right: &Value, keypath: &mut Vec<Value>, stanzas: &mut Vec<Value>) {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for (key, l_val) in l {
                keypath.push(Value::String(key.clone()));
                match r.get(key) {
                    None => stanzas.push(json!([keypath.clone()])),
                    Some(r_val) if l_val != r_val => diff_into(l_val, r_val, keypath, stanzas),
                    Some(_) => {}
                }
                keypath.pop();
            }
            for (key, r_val) in r {
                if !l.contains_key(key) {
                    keypath.push(Value::String(key.clone()));
                    stanzas.push(json!([keypath.clone(), r_val]));
                    keypath.pop();
                }
            }
        }
        (Value::Array(l), Value::Array(r)) => {
            let common = l.len().min(r.len());
            for i in 0..common {
                if l[i] != r[i] {
                    keypath.push(json!(i));
                    diff_into(&l[i], &r[i], keypath, stanzas);
                    keypath.pop();
                }
            }
            for (i, r_val) in r.iter().enumerate().skip(common) {
                keypath.push(json!(i));
                stanzas.push(json!([keypath.clone(), r_val]));
                keypath.pop();
            }
            for i in (common..l.len()).rev() {
                keypath.push(json!(i));
                stanzas.push(json!([keypath.clone()]));
                keypath.pop();
            }
        }
        _ => {
            if left != right {
                stanzas.push(json!([keypath.clone(), right]));
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Same semantics as `JSON_delta.patchStanza` in `app/src/vendor/json_delta.js`.
    pub fn patch(mut struc: Value, diff: &[Value]) -> Value {
        for stanza in diff {
            let stanza = stanza.as_array().expect("stanza is an array");
            let keypath = stanza[0].as_array().expect("keypath is an array").clone();
            struc = patch_stanza(struc, &keypath, stanza.get(1));
        }
        struc
    }

    fn patch_stanza(mut struc: Value, keypath: &[Value], value: Option<&Value>) -> Value {
        match keypath {
            [] => value.expect("root stanza has a value").clone(),
            [key] => {
                match (&mut struc, value) {
                    (Value::Array(a), None) => {
                        a.remove(key.as_u64().unwrap() as usize);
                    }
                    (Value::Array(a), Some(v)) => {
                        let i = key.as_u64().unwrap() as usize;
                        if i == a.len() {
                            a.push(v.clone());
                        } else {
                            a[i] = v.clone();
                        }
                    }
                    (Value::Object(o), None) => {
                        o.remove(key.as_str().unwrap());
                    }
                    (Value::Object(o), Some(v)) => {
                        o.insert(key.as_str().unwrap().to_string(), v.clone());
                    }
                    _ => panic!("cannot patch scalar"),
                }
                struc
            }
            [key, rest @ ..] => {
                let child = match key {
                    Value::String(k) => struc[k.as_str()].take(),
                    _ => struc[key.as_u64().unwrap() as usize].take(),
                };
                let patched = patch_stanza(child, rest, value);
                match key {
                    Value::String(k) => struc[k.as_str()] = patched,
                    _ => struc[key.as_u64().unwrap() as usize] = patched,
                }
                struc
            }
        }
    }

    fn assert_roundtrip(left: Value, right: Value) {
        let d = diff(&left, &right);
        assert_eq!(patch(left.clone(), &d), right, "diff: {d:?}");
    }

    #[test]
    fn equal_values_give_empty_diff() {
        let v = json!({"a": [1, 2, {"b": null}], "c": "x"});
        assert!(diff(&v, &v).is_empty());
    }

    #[test]
    fn object_add_change_delete() {
        assert_roundtrip(
            json!({"keep": 1, "change": {"x": 1}, "del": true}),
            json!({"keep": 1, "change": {"x": 2, "y": [1]}, "add": "new"}),
        );
    }

    #[test]
    fn array_grow_and_shrink() {
        assert_roundtrip(json!({"a": [1, 2, 3, 4, 5]}), json!({"a": [1, 9]}));
        assert_roundtrip(json!({"a": [1]}), json!({"a": [1, 2, 3]}));
        assert_roundtrip(json!([{"n": 1}, {"n": 2}]), json!([{"n": 3}]));
    }

    #[test]
    fn root_type_change() {
        assert_roundtrip(json!({"a": 1}), json!([1, 2]));
        assert_roundtrip(json!(1), json!("x"));
    }

    #[test]
    fn array_deletes_are_descending() {
        let d = diff(&json!([1, 2, 3]), &json!([1]));
        assert_eq!(d, vec![json!([[2]]), json!([[1]])]);
    }
}
