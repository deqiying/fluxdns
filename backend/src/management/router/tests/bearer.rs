use super::*;

#[tokio::test]
async fn refresh_requires_cookie_and_origin_and_never_accepts_bearer_or_query_as_refresh() {
    let (services, root, _) = test_services();
    let issued = services.sessions.issue("admin".to_owned()).unwrap();
    let cookie = format!("{}={}", services.sessions.cookie_name(), issued.token);
    let app = build_router(Arc::clone(&services));
    for (header, query, expected) in [
        (None, false, StatusCode::UNAUTHORIZED),
        (None, true, StatusCode::UNAUTHORIZED),
        (
            Some((
                AUTHORIZATION,
                format!("Bearer {}", issued.view.access_token),
            )),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            Some((
                COOKIE,
                format!(
                    "{}={}",
                    services.sessions.cookie_name(),
                    issued.view.access_token
                ),
            )),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (Some((COOKIE, cookie.clone())), false, StatusCode::OK),
    ] {
        let path = if query {
            format!("/api/v2/auth/refresh?token={}", issued.token)
        } else {
            "/api/v2/auth/refresh".to_owned()
        };
        let mut request = post(&path, "");
        if let Some((key, value)) = header {
            request.headers_mut().insert(key, value.parse().unwrap());
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!body.to_string().contains(&issued.token));
        if expected == StatusCode::OK {
            assert_eq!(body["access_token"], issued.view.access_token);
            assert_eq!(body["session"]["user"]["name"], "admin");
        }
    }
    for foreign in [None, Some("https://foreign.example.test")] {
        let mut request = post("/api/v2/auth/refresh", "");
        request
            .headers_mut()
            .insert(COOKIE, cookie.parse().unwrap());
        request.headers_mut().remove(ORIGIN);
        if let Some(value) = foreign {
            request.headers_mut().insert(ORIGIN, value.parse().unwrap());
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    drop(app);
    drop(services);
    cleanup_test_root(&root);
}

#[tokio::test]
async fn duplicate_malformed_and_revoked_authorization_never_fall_back_to_cookie() {
    let (services, root, _) = test_services();
    let issued = services.sessions.issue("admin".to_owned()).unwrap();
    let cookie = format!("{}={}", services.sessions.cookie_name(), issued.token);
    let app = build_router(Arc::clone(&services));
    for headers in [
        vec![
            format!("Bearer {}", issued.view.access_token),
            format!("Bearer {}", issued.view.access_token),
        ],
        vec![format!("Bearer  {}", issued.view.access_token)],
        vec![format!("Bearer {},other", issued.view.access_token)],
        vec!["Bearer ".to_owned()],
        vec![format!("Basic {}", issued.view.access_token)],
    ] {
        let mut request = Request::builder()
            .uri("/api/v2/auth/session")
            .header(COOKIE, &cookie);
        for value in headers {
            request = request.header(AUTHORIZATION, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    services.sessions.revoke(&issued.view.access_token);
    let request = Request::builder()
        .uri("/api/v2/auth/session")
        .header(COOKIE, &cookie)
        .header(
            AUTHORIZATION,
            format!("Bearer {}", issued.view.access_token),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    drop(services);
    cleanup_test_root(&root);
}
