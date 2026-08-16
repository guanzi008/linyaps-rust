use serde_json::Value;

pub fn apply_json_patch(document: &Value, operations: &[Value]) -> Result<Value, String> {
    let mut patched = document.clone();
    for (index, operation) in operations.iter().enumerate() {
        apply_operation(&mut patched, operation)
            .map_err(|error| format!("patch operation {index} failed: {error}"))?;
    }
    Ok(patched)
}

fn apply_operation(document: &mut Value, operation: &Value) -> Result<(), String> {
    let operation = operation
        .as_object()
        .ok_or_else(|| "operation is not an object".to_string())?;
    let name = operation
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "operation has no string 'op'".to_string())?;
    let path = operation
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "operation has no string 'path'".to_string())?;
    let path = parse_pointer(path)?;
    match name {
        "add" => {
            let value = operation
                .get("value")
                .cloned()
                .ok_or_else(|| "add operation has no 'value'".to_string())?;
            add(document, &path, value)
        }
        "remove" => remove(document, &path).map(drop),
        "replace" => {
            let value = operation
                .get("value")
                .cloned()
                .ok_or_else(|| "replace operation has no 'value'".to_string())?;
            replace(document, &path, value)
        }
        "move" => {
            let from = operation
                .get("from")
                .and_then(Value::as_str)
                .ok_or_else(|| "move operation has no string 'from'".to_string())?;
            let from = parse_pointer(from)?;
            if path.len() > from.len() && path.starts_with(&from) {
                return Err("move destination is a child of its source".to_string());
            }
            let value = remove(document, &from)?;
            add(document, &path, value)
        }
        "copy" => {
            let from = operation
                .get("from")
                .and_then(Value::as_str)
                .ok_or_else(|| "copy operation has no string 'from'".to_string())?;
            let from = parse_pointer(from)?;
            let value = value_at(document, &from)?.clone();
            add(document, &path, value)
        }
        "test" => {
            let expected = operation
                .get("value")
                .ok_or_else(|| "test operation has no 'value'".to_string())?;
            if value_at(document, &path)? == expected {
                Ok(())
            } else {
                Err("test operation failed".to_string())
            }
        }
        other => Err(format!("unsupported operation '{other}'")),
    }
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, String> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let pointer = pointer
        .strip_prefix('/')
        .ok_or_else(|| "JSON pointer must be empty or start with '/'".to_string())?;
    pointer.split('/').map(decode_pointer_token).collect()
}

fn decode_pointer_token(token: &str) -> Result<String, String> {
    let mut decoded = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            Some(character) => return Err(format!("invalid JSON pointer escape '~{character}'")),
            None => return Err("incomplete JSON pointer escape".to_string()),
        }
    }
    Ok(decoded)
}

fn value_at<'a>(document: &'a Value, path: &[String]) -> Result<&'a Value, String> {
    let mut current = document;
    for token in path {
        current = match current {
            Value::Object(object) => object
                .get(token)
                .ok_or_else(|| format!("object member '{token}' does not exist"))?,
            Value::Array(array) => {
                let index = array_index(token, false, array.len())?;
                array
                    .get(index)
                    .ok_or_else(|| format!("array index {index} is out of bounds"))?
            }
            _ => return Err(format!("cannot traverse through '{token}'")),
        };
    }
    Ok(current)
}

fn parent_mut<'a>(document: &'a mut Value, path: &[String]) -> Result<&'a mut Value, String> {
    let mut current = document;
    for token in path {
        current = match current {
            Value::Object(object) => object
                .get_mut(token)
                .ok_or_else(|| format!("object member '{token}' does not exist"))?,
            Value::Array(array) => {
                let index = array_index(token, false, array.len())?;
                array
                    .get_mut(index)
                    .ok_or_else(|| format!("array index {index} is out of bounds"))?
            }
            _ => return Err(format!("cannot traverse through '{token}'")),
        };
    }
    Ok(current)
}

fn add(document: &mut Value, path: &[String], value: Value) -> Result<(), String> {
    let Some((name, parent_path)) = path.split_last() else {
        *document = value;
        return Ok(());
    };
    match parent_mut(document, parent_path)? {
        Value::Object(object) => {
            object.insert(name.clone(), value);
            Ok(())
        }
        Value::Array(array) => {
            let index = array_index(name, true, array.len())?;
            array.insert(index, value);
            Ok(())
        }
        _ => Err("add destination parent is not a container".to_string()),
    }
}

fn remove(document: &mut Value, path: &[String]) -> Result<Value, String> {
    let Some((name, parent_path)) = path.split_last() else {
        return Ok(std::mem::take(document));
    };
    match parent_mut(document, parent_path)? {
        Value::Object(object) => object
            .remove(name)
            .ok_or_else(|| format!("object member '{name}' does not exist")),
        Value::Array(array) => {
            let index = array_index(name, false, array.len())?;
            if index >= array.len() {
                Err(format!("array index {index} is out of bounds"))
            } else {
                Ok(array.remove(index))
            }
        }
        _ => Err("remove destination parent is not a container".to_string()),
    }
}

fn replace(document: &mut Value, path: &[String], value: Value) -> Result<(), String> {
    let Some((name, parent_path)) = path.split_last() else {
        *document = value;
        return Ok(());
    };
    match parent_mut(document, parent_path)? {
        Value::Object(object) => {
            let destination = object
                .get_mut(name)
                .ok_or_else(|| format!("object member '{name}' does not exist"))?;
            *destination = value;
            Ok(())
        }
        Value::Array(array) => {
            let index = array_index(name, false, array.len())?;
            let destination = array
                .get_mut(index)
                .ok_or_else(|| format!("array index {index} is out of bounds"))?;
            *destination = value;
            Ok(())
        }
        _ => Err("replace destination parent is not a container".to_string()),
    }
}

fn array_index(token: &str, allow_end: bool, length: usize) -> Result<usize, String> {
    if allow_end && token == "-" {
        return Ok(length);
    }
    if token.is_empty()
        || (token.len() > 1 && token.starts_with('0'))
        || !token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid array index '{token}'"));
    }
    let index = token
        .parse::<usize>()
        .map_err(|_| format!("invalid array index '{token}'"))?;
    if index > length || (!allow_end && index == length) {
        return Err(format!("array index {index} is out of bounds"));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn implements_all_rfc6902_operations() {
        let document = json!({"items":["a","b"],"nested":{"old":1},"slash/key":true});
        let operations = vec![
            json!({"op":"test","path":"/slash~1key","value":true}),
            json!({"op":"add","path":"/items/-","value":"c"}),
            json!({"op":"replace","path":"/nested/old","value":2}),
            json!({"op":"copy","from":"/nested/old","path":"/nested/copy"}),
            json!({"op":"move","from":"/items/0","path":"/items/2"}),
            json!({"op":"remove","path":"/slash~1key"}),
        ];
        assert_eq!(
            apply_json_patch(&document, &operations).unwrap(),
            json!({"items":["b","c","a"],"nested":{"old":2,"copy":2}})
        );
    }

    #[test]
    fn failed_patch_does_not_modify_source_document() {
        let document = json!({"value":1});
        assert!(
            apply_json_patch(
                &document,
                &[json!({"op":"replace","path":"/missing","value":2})]
            )
            .is_err()
        );
        assert_eq!(document, json!({"value":1}));
    }
}
