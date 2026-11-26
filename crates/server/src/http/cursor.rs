//! Cursor pagination (F025): the wire's slicing helper. Cursors are
//! opaque strings (`c<offset>`); the services return deterministic
//! orderings (documented per route), so an offset cursor is stable
//! between consecutive pages of one listing.
//!
//! `limit` defaults to 50 and is capped at 200: a client asking for
//! unbounded pages is the slow-consumer problem over REST.

use axum::http::StatusCode;

use super::problem::Problem;

/// The default page size.
pub const DEFAULT_LIMIT: usize = 50;

/// The maximum page size.
pub const MAX_LIMIT: usize = 200;

/// Encodes an offset into an opaque cursor.
pub fn encode(offset: usize) -> String {
    format!("c{offset}")
}

/// Decodes an opaque cursor into its offset (400 problem when the
/// client mangles it — the detail says `cursor` and nothing else).
pub fn decode(raw: &str) -> Result<usize, Problem> {
    let offset = raw
        .strip_prefix('c')
        .and_then(|digits| digits.parse::<usize>().ok())
        .ok_or_else(|| {
            Problem::new(
                "invalid-cursor",
                StatusCode::BAD_REQUEST,
                "invalid request",
                Some("cursor is not a valid page cursor".to_owned()),
            )
        })?;
    Ok(offset)
}

/// Parses a `limit` query value: default, capped, 400 when not a
/// positive number.
pub fn parse_limit(raw: Option<&str>) -> Result<usize, Problem> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_LIMIT);
    };
    let limit = raw
        .parse::<usize>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| {
            Problem::new(
                "invalid-limit",
                StatusCode::BAD_REQUEST,
                "invalid request",
                Some("limit must be a positive integer".to_owned()),
            )
        })?;
    Ok(limit.min(MAX_LIMIT))
}

/// One bounded page of a listing.
#[derive(Debug, PartialEq, Eq)]
pub struct Page<T> {
    /// The items on this page.
    pub items: Vec<T>,
    /// The cursor to request the next page with (None when the
    /// listing is exhausted).
    pub next_cursor: Option<String>,
}

/// Slices `items` (already sorted by the service) at `offset` with
/// `limit`, producing the follow-up cursor when more remain.
pub fn paginate<T>(items: Vec<T>, offset: usize, limit: usize) -> Page<T> {
    let total = items.len();
    if offset >= total {
        return Page {
            items: Vec::new(),
            next_cursor: None,
        };
    }
    let end = (offset + limit).min(total);
    let page = items.into_iter().skip(offset).take(end - offset).collect();
    Page {
        next_cursor: (end < total).then(|| encode(end)),
        items: page,
    }
}
