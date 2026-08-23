//! Hand-rolled AWS Signature Version 4 (hmac-sha256 based).
//!
//! Avoids the multi-crate AWS SDK; verified against the official AWS test
//! vector `get-vanilla-query-order-key-case` from the SigV4 test suite.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[derive(Debug, Clone)]
pub struct SigV4Request<'a> {
    pub method: &'a str,
    /// Fully qualified host, e.g. `bedrock-runtime.us-east-1.amazonaws.com`.
    pub host: &'a str,
    /// URI-encoded path (already escaped, starts with `/`).
    pub path: &'a str,
    /// Canonical query string (params sorted, URI-encoded), or empty.
    pub query: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    /// Optional session token (`x-amz-security-token`).
    pub session_token: Option<&'a str>,
    /// Hex sha256 of the request body (`sha256_hex` of the empty string for GET).
    pub payload_hash: &'a str,
    /// Additional signed headers (e.g. `content-type`); sorted automatically.
    pub extra_headers: &'a [(&'a str, &'a str)],
}

pub struct SignedHeaders {
    pub amz_date: String,
    pub authorization: String,
    pub security_token: Option<String>,
}

/// Compute SigV4 headers for a request at the given instant.
pub fn sign(request: &SigV4Request<'_>, amz_date: &str) -> SignedHeaders {
    // amz_date format: YYYYMMDDTHHMMSSZ; scope date is its first 8 chars.
    let date_stamp = &amz_date[..8];

    let mut pairs: Vec<(&str, String)> = vec![
        ("host", request.host.to_string()),
        ("x-amz-date", amz_date.to_string()),
    ];
    for (name, value) in request.extra_headers {
        pairs.push((name, (*value).to_string()));
    }
    if let Some(token) = request.session_token {
        pairs.push(("x-amz-security-token", token.to_string()));
    }
    pairs.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = pairs.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed_headers = pairs.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        request.path,
        request.query,
        canonical_headers,
        signed_headers,
        request.payload_hash
    );

    let scope = format!(
        "{}/{}/{}/aws4_request",
        date_stamp, request.region, request.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", request.secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, request.region.as_bytes());
    let k_service = hmac_sha256(&k_region, request.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        request.access_key, scope, signed_headers, signature
    );

    SignedHeaders {
        amz_date: amz_date.to_string(),
        authorization,
        security_token: request.session_token.map(String::from),
    }
}

/// Current `x-amz-date` timestamp (UTC).
pub fn amz_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_PAYLOAD_HASH: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn matches_aws_official_vector_get_vanilla_query_order_key_case() {
        // https://docs.aws.amazon.com/general/latest/gr/sigv4-test-suite.html
        let req = SigV4Request {
            method: "GET",
            host: "iam.amazonaws.com",
            path: "/",
            query: "Action=ListUsers&Version=2010-05-08",
            region: "us-east-1",
            service: "iam",
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: None,
            payload_hash: EMPTY_PAYLOAD_HASH,
            extra_headers: &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
        };
        let signed = sign(&req, "20150830T123600Z");
        assert_eq!(signed.amz_date, "20150830T123600Z");
        // Official AWS docs example (IAM ListUsers):
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn includes_session_token_when_present() {
        let req = SigV4Request {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/x/converse",
            query: "",
            region: "us-east-1",
            service: "bedrock",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: Some("TOKEN"),
            payload_hash: EMPTY_PAYLOAD_HASH,
            extra_headers: &[],
        };
        let signed = sign(&req, "20260823T000000Z");
        assert!(signed.security_token.is_some());
        assert!(signed
            .authorization
            .contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
    }
}
