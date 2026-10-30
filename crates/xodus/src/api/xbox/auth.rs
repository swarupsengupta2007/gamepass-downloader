use crate::models::xbox::{
    UserAuthProperties, UserAuthRequest, XstsPropertyBag, XstsRequest, XstsResponse,
};

pub async fn authenticate_xbox_user(
    client: &reqwest::Client,
    rps_ticket: String,
) -> reqwest::Result<XstsResponse> {
    // The ticket prefix is undocumented and has changed: since 2026-10-28 the
    // live.com token service labels the MBI_SSL compact ticket `d=`, and
    // user.auth.xboxlive.com refuses it that way (400, x-err 5) but accepts the
    // same ticket as `t=`. Other clients have needed no prefix. A malformed
    // ticket is a 400, so each form is tried in turn only after one.
    let mut resp = send_user_auth(client, rps_ticket.clone()).await?;
    for form in ticket_forms(&rps_ticket).into_iter().skip(1) {
        if resp.status() != reqwest::StatusCode::BAD_REQUEST {
            break;
        }
        refusal_diagnostic("xbox-user", &resp);
        resp = send_user_auth(client, form).await?;
    }
    refusal_diagnostic("xbox-user", &resp);
    resp.error_for_status()?.json().await
}

/// The ticket as issued, then as `t=`, then without a prefix, each once.
fn ticket_forms(ticket: &str) -> Vec<String> {
    let body = ticket.strip_prefix("d=").or_else(|| ticket.strip_prefix("t=")).unwrap_or(ticket);
    let mut forms = vec![ticket.to_owned()];
    for form in [format!("t={body}"), body.to_owned()] {
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

#[cfg(test)]
#[test]
fn ticket_forms_try_each_prefix_once() {
    assert_eq!(ticket_forms("d=EwA"), ["d=EwA", "t=EwA", "EwA"]);
    assert_eq!(ticket_forms("t=EwA"), ["t=EwA", "EwA"]);
    assert_eq!(ticket_forms("EwA"), ["EwA", "t=EwA"]);
}

async fn send_user_auth(client: &reqwest::Client, rps_ticket: String) -> reqwest::Result<reqwest::Response> {
    let body = UserAuthRequest {
        relying_party: "http://auth.xboxlive.com".to_string(),
        token_type: "JWT".to_string(),
        properties: UserAuthProperties {
            auth_method: "RPS".to_string(),
            site_name: "user.auth.xboxlive.com".to_string(),
            rps_ticket,
        },
    };
    client
        .post("https://user.auth.xboxlive.com/user/authenticate")
        .header("Content-Type", "application/json")
        .header("x-xbl-contract-version", "1")
        .json(&body)
        .send()
        .await
}

/// Xbox auth refusals carry the reason only in the status and the `x-err`
/// header (for example 2148916233: no Xbox profile). Neither is secret.
fn refusal_diagnostic(stage: &str, resp: &reqwest::Response) {
    if resp.status().is_success() {
        return;
    }
    let xerr = resp.headers().get("x-err").and_then(|v| v.to_str().ok()).unwrap_or("-");
    let xerr: String = xerr.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    eprintln!("NEXUS_SESSION_DIAGNOSTIC_V1 {stage}-refused {} {xerr}", resp.status().as_u16());
}

pub async fn request_xsts_token(
    client: &reqwest::Client,
    token: String,
    relying_party: &str,
) -> reqwest::Result<XstsResponse> {
    let body = XstsRequest {
        relying_party: Some(relying_party.to_string()),
        token_type: Some("JWT".to_string()),
        properties: XstsPropertyBag {
            user_tokens: Some(vec![token]),
            sandbox_id: Some("RETAIL".to_string()),
            delegation_token: None,
            service_token: None,
        },
    };

    let resp = client
        .post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .header("Content-Type", "application/json")
        .header("x-xbl-contract-version", "1")
        .json(&body)
        .send()
        .await?;
    refusal_diagnostic("xsts", &resp);
    resp.error_for_status()?.json().await
}

pub fn get_xsts_auth_header(
    xsts: XstsResponse,
) -> Result<String, crate::models::secrets::InvalidTokenResponse> {
    use crate::models::secrets::InvalidTokenResponse;
    let uhs = xsts
        .user_hash()
        .filter(|value| !value.is_empty())
        .ok_or(InvalidTokenResponse)?;
    if xsts.token.is_empty() {
        return Err(InvalidTokenResponse);
    }
    Ok(format!("XBL3.0 x={uhs};{}", xsts.token))
}
