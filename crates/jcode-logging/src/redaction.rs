use super::truncate;

pub(super) fn redact_field_value(key: &str, value: &str) -> String {
    if is_sensitive_key(key) {
        return "<redacted>".to_string();
    }
    sanitize_log_value(value)
}

pub(super) fn sanitize_log_value(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let value = redact_url_queries(&value);
    let value = redact_sensitive_assignments(&value);
    let value = redact_known_secret_tokens(&value);
    truncate(&value, 160)
}

fn redact_url_queries(value: &str) -> String {
    value
        .split_whitespace()
        .map(redact_url_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_url_token(token: &str) -> String {
    let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| token.find(scheme))
        .min()
    else {
        return token.to_string();
    };
    let url = &token[start..];
    let Some(query_start) = url.find('?') else {
        return token.to_string();
    };
    format!("{}{}?<redacted>", &token[..start], &url[..query_start])
}

fn normalized_log_key(key: &str) -> String {
    key.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn is_safe_metadata_key(key: &str) -> bool {
    [
        "_id",
        "_name",
        "_count",
        "_len",
        "_length",
        "_present",
        "_configured",
        "_available",
        "_enabled",
        "_source",
        "_type",
    ]
    .iter()
    .any(|suffix| key.ends_with(suffix))
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalized_log_key(key);
    if key.is_empty() || is_safe_metadata_key(&key) {
        return false;
    }

    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "key"
            | "token"
            | "secret"
            | "credential"
            | "authorization"
            | "password"
            | "device_code"
            | "magic_link"
            | "callback_url"
            | "approval_url"
            | "checkout_url"
            | "portal_url"
            | "checkout_session_url"
            | "portal_session_url"
            | "client_secret"
            | "access_token"
            | "refresh_token"
            | "webhook_secret"
            | "webhook_signing_secret"
            | "private_key"
            | "signing_key"
            | "cookie"
            | "set_cookie"
            | "auth_header"
            | "proxy_authorization"
            | "auth_code"
            | "oauth_code"
            | "code_verifier"
            | "code_challenge"
            | "session_url"
    ) || key.starts_with("authorization_")
        || key.starts_with("api_key_")
        || key.ends_with("_api_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_credential")
        || key.ends_with("_device_code")
        || key.ends_with("_magic_link")
        || key.ends_with("_magic_link_url")
        || key.ends_with("_callback_url")
        || key.ends_with("_approval_url")
        || key.ends_with("_checkout_url")
        || key.ends_with("_portal_url")
        || key.ends_with("_session_url")
        || key.ends_with("_checkout_session_url")
        || key.ends_with("_portal_session_url")
        || key.ends_with("_client_secret")
        || key.ends_with("_access_token")
        || key.ends_with("_refresh_token")
        || key.ends_with("_secret_key")
        || key.ends_with("_private_key")
        || key.ends_with("_signing_key")
        || key.ends_with("_auth_code")
        || key.ends_with("_oauth_code")
        || key.ends_with("_code_verifier")
        || key.ends_with("_code_challenge")
}

fn redact_sensitive_assignments(value: &str) -> String {
    let mut output = Vec::new();
    let mut redact_next = 0u8;

    for token in value.split_whitespace() {
        if redact_next > 0 {
            if token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("token") {
                output.push(token.to_string());
            } else {
                output.push("<redacted>".to_string());
            }
            redact_next -= 1;
            continue;
        }

        if token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("token") {
            output.push(token.to_string());
            redact_next = 1;
            continue;
        }

        if let Some((key, separator, assigned_value)) = split_assignment(token)
            && is_sensitive_key(key)
        {
            output.push(format!("{key}{separator}<redacted>"));
            if assigned_value.is_empty() {
                redact_next = 2;
            }
            continue;
        }

        if let Some(key) = token.strip_suffix(':')
            && is_sensitive_key(key)
        {
            output.push(format!("{key}:"));
            redact_next = 2;
            continue;
        }

        output.push(token.to_string());
    }

    output.join(" ")
}

fn redact_known_secret_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;

    for (offset, _) in value.char_indices() {
        if offset < cursor {
            continue;
        }
        let Some(end) = known_secret_span(&value[offset..]) else {
            continue;
        };
        output.push_str(&value[cursor..offset]);
        output.push_str("<redacted>");
        cursor = offset + end;
    }

    output.push_str(&value[cursor..]);
    output
}

fn known_secret_span(value: &str) -> Option<usize> {
    if value.starts_with("-----BEGIN ") && value.contains("PRIVATE KEY-----") {
        let end_start = value.find("-----END ")?;
        let end_body_start = end_start + "-----END ".len();
        let end_body = value[end_body_start..].find("-----")?;
        return Some(end_body_start + end_body + 5);
    }

    for (prefix, minimum_suffix, allowed) in [
        ("sk-ant-oat01-", 8, is_token_char),
        ("sk-ant-ort01-", 8, is_token_char),
        ("sk-or-v1-", 8, is_token_char),
        ("sk-proj-", 8, is_token_char),
        ("sk_live_", 8, is_token_char),
        ("rk_live_", 8, is_token_char),
        ("sk_test_", 8, is_token_char),
        ("rk_test_", 8, is_token_char),
        ("whsec_", 8, is_token_char),
        ("jck_live_", 8, is_token_char),
        ("ghp_", 8, is_token_char),
        ("gho_", 8, is_token_char),
        ("ghu_", 8, is_token_char),
        ("ghs_", 8, is_token_char),
        ("github_pat_", 8, is_token_char),
        ("xoxb-", 8, is_token_char),
        ("xoxp-", 8, is_token_char),
        ("xoxa-", 8, is_token_char),
        ("xoxr-", 8, is_token_char),
        ("npm_", 8, is_token_char),
    ] {
        if let Some(suffix) = value.strip_prefix(prefix) {
            let suffix_len = suffix.bytes().take_while(|byte| allowed(*byte)).count();
            if suffix_len >= minimum_suffix {
                return Some(prefix.len() + suffix_len);
            }
        }
    }

    if let Some(suffix) = value.strip_prefix("ya29.") {
        let suffix_len = suffix
            .bytes()
            .take_while(|byte| is_token_char(*byte) || *byte == b'.')
            .count();
        if suffix_len >= 20 {
            return Some("ya29.".len() + suffix_len);
        }
    }

    if let Some(suffix) = value.strip_prefix("AIza") {
        let suffix_len = suffix
            .bytes()
            .take_while(|byte| is_token_char(*byte))
            .count();
        if suffix_len >= 20 {
            return Some("AIza".len() + suffix_len);
        }
    }

    if value.starts_with("AKIA")
        && value.as_bytes().get(4..20).is_some_and(|suffix| {
            suffix.len() == 16
                && suffix
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
        && value
            .as_bytes()
            .get(20)
            .is_none_or(|byte| !is_token_char(*byte))
    {
        return Some(20);
    }

    if value.starts_with("eyJ")
        && let Some(span) = jwt_like_span(value)
    {
        return Some(span);
    }

    None
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn jwt_like_span(value: &str) -> Option<usize> {
    let mut cursor = 0;
    for index in 0..3 {
        let start = cursor;
        while value
            .as_bytes()
            .get(cursor)
            .is_some_and(|byte| is_jwt_char(*byte))
        {
            cursor += 1;
        }
        if cursor - start < 10 {
            return None;
        }
        if index < 2 {
            if value.as_bytes().get(cursor) != Some(&b'.') {
                return None;
            }
            cursor += 1;
        }
    }
    Some(cursor)
}

fn is_jwt_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn split_assignment(token: &str) -> Option<(&str, &str, &str)> {
    let (index, separator) = token
        .char_indices()
        .find(|(_, character)| *character == '=' || *character == ':')?;
    let separator_end = index + separator.len_utf8();
    Some((
        &token[..index],
        &token[index..separator_end],
        &token[separator_end..],
    ))
}

pub(super) fn sanitize_payload_for_log(value: &str, max_chars: usize) -> String {
    let sanitized = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(mut payload) => {
            redact_json_value(&mut payload);
            serde_json::to_string(&payload).unwrap_or_else(|_| "<unserializable>".to_string())
        }
        Err(_) => sanitize_log_value(value),
    };
    truncate(&sanitized, max_chars)
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String("<redacted>".to_string());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        serde_json::Value::String(value) => {
            *value = sanitize_log_value(value);
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}
