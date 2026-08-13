// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Real event bodies captured from chain, one constant per event type.
//!
//! These are the bytes an indexer actually receives. Every assertion built on them
//! confirms the payload's shape by observation rather than by intention — which is
//! the whole point of the replay wave, and the reason a synthetic body must never
//! be added to this file.
//!
//! Each constant carries where it came from. A shellnet redeploy retires the message
//! id, and that is fine: the base64 is self-contained and the id is a historical
//! reference, not a dependency.
//!
//! Every body below is `rank 1` from the wave-4 harvest journal
//! (`specs/2026-08-13-wave4-harvest.md`, Task 1 of this wave): the sole or longest
//! candidate of its event type inside the fresh capture window
//! (`created_at_chain >= 2026-08-08`), with `InferenceOrderCancelRejected` captured
//! exactly on the boundary date and counted as fresh. Length is a proxy for a
//! multi-cell payload — the descent that carries the prefix offset into a
//! continuation cell is the thing a single-cell body leaves untested. All 59
//! candidates in the journal, this body included, passed decode validation against
//! the `decoded` snapshot production recorded by the decoder at capture time
//! (journal Step 7); nothing here was hand-fixed to pass.
//!
//! `TokenContract.*` constants belong to a different task (owner: Task 5, in the
//! same file) and are not added here.

/// `InferenceOrderBook.InferenceOrderPlaced`, captured from event message
/// `1269194757f243fb77f116d629f86a7431902c5330db592a528fd1be7548dc0a` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003e8`.
///
/// Inference event ids are unique across the loaded ABIs (`decoder.rs`'s
/// `all_inference_events_resolve_uniquely_by_id` proves it), so decoding this body
/// needs no `dst` — `decode_event_body(INFERENCE_ORDER_PLACED, None)` resolves it
/// unambiguously. The `dst` above is recorded for provenance only; routing by `dst`
/// matters for `TokenContract.ContractDeployed`, which collides on event id with
/// `RootModel.ContractDeployed` — a different fixture, owned by a different task.
///
/// Chosen as the longest body of its type in the source: length is the proxy for a
/// multi-cell payload, and a multi-cell body is the one that exercises carrying the
/// prefix offset into a continuation cell — the descent a single-cell body leaves
/// untested.
pub const INFERENCE_ORDER_PLACED: &str = "te6ccgEBAwEAmgABiWDHFLoAAAAAAAAAAAAAAAAAAAIWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFloLwAAAAAAAAAAAAAAAAAAAAACQAEBQ4Ac5gwN5iwntKJmSzeCwvw5YbuXPp1Njnr4tAFKZN5lXVACAFWAEuHiAOHYgk3WaMpg5GUKq+sYYOxSwlQqbvvsyqrpFMjAAAAADU/C54AQ";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_PLACED`] at capture
/// time (harvest journal, Task 1 Step 5) — the whole-payload comparison target used by
/// `decode_real_bodies.rs` alongside the per-field asserts.
pub const HARVESTED_ORDER_PLACED_DECODED: &str = r#"{"note":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea","flags":"0","isBuy":false,"price":"0x00000000000000000000000000000000000000000000000000000000b2d05e00","ticks":"4","orderId":"534","deadline":"1786648380","tokenContract":"0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"}"#;

/// `InferenceOrderBook.InferenceFilled`, captured from event message
/// `adad773107dc8aff53d52a54ea3b4cbbba6b27b54d9ce7dd9ab9ec7dcdb81ed6` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003eb`.
///
/// The longest body in the whole wave-4 harvest (280 base64 chars) — the strongest
/// exercise of the multi-cell descent available among the 59 harvested candidates.
pub const INFERENCE_FILLED: &str = "te6ccgEBBAEAxQABqEDU3wcAAAAAAAAAAAAAAAAAAAIUAAAAAAAAAAAAAAAAAAACFQAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAstBeAAEBQ4AKNy/qEBMEZZMyP4l90vavDVDoo+1k8DtmFrhzsaCk0TACAUOAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOwAwBDgBzmDA3mLCe0omZLN4LC/Dlhu5c+nU2Oevi0AUpk3mVdUA==";

/// The `decoded` snapshot production recorded for [`INFERENCE_FILLED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_FILLED_DECODED: &str = r#"{"ticks":"8","makerId":"532","takerId":"533","sellerTC":"0:51b97f508098232c9991fc4bee97b5786a87451f6b2781db30b5c39d8d052689","buyerNote":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","sellerNote":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea","clearingPrice":"0x00000000000000000000000000000000000000000000000000000000b2d05e00"}"#;

/// `InferenceOrderBook.InferenceExecuted`, captured from event message
/// `bc15baa85fe19129f34c615982c0da71c66dc475594ba443041af6796f497b4d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003ec`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_EXECUTED: &str = "te6ccgEBAQEARgAAiDrthbkAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAALLQXgAAAAAAAAAAAAAAAAW6RjYA";

/// The `decoded` snapshot production recorded for [`INFERENCE_EXECUTED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_EXECUTED_DECODED: &str = r#"{"cost":"24600000000","ticks":"8","clearingPrice":"0x00000000000000000000000000000000000000000000000000000000b2d05e00"}"#;

/// `InferenceOrderBook.InferenceOrderBookDeployed`, captured from event message
/// `e1cccb7ddf6314f97645b851c05588d42ab98e0eb5d1b3848607884155c35c61` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f0`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_BOOK_DEPLOYED: &str = "te6ccgEBAgEAegABiyAMXJyAEbwN6jH/wYQuEaioSLkzwR5jVeLjF2h4QprAb+JSLeGo+fL+lZsvt7p39iQit9WIIK03J92kaPEDTopdHOnkCNABAF5xd2VuLS1xd2VuMy0tMzJiLWlzc3VlMjY0LWZhaWxjbG9zZWQtMTc4NjYzNjE5NA==";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_BOOK_DEPLOYED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_BOOK_DEPLOYED_DECODED: &str = r#"{"note":"0:8de06f518ffe0c21708d454245c99e08f31aaf1718bb43c214d6037f12916f0d","modelHash":"0x47cf97f4acd97dbdd3bfb12115beac410569b93eed2347881a7452e8e74f2046","modelName":"qwen--qwen3--32b-issue264-failclosed-1786636194"}"#;

/// `InferenceOrderBook.InferenceOrderCancelled`, captured from event message
/// `bafa1344a5fb8d7ee0c2cb8dd6d760a8d9578ec2b018c940e49a7348e2201778` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003e9`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_CANCELLED: &str = "te6ccgEBAQEASAAAi32/6dQAAAAAAAAAAAAAAAAAAAIQAAAAAAAAAAAAAAAAAAAAAIAUYbHKpTOwM6xtPrpWbMqpwLOWFRu6nEsTfO3J/p02MfA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_CANCELLED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_CANCELLED_DECODED: &str = r#"{"note":"0:a30d8e55299d819d6369f5d2b366554e059cb0a8ddd4e2589be76e4ff4e9b18f","orderId":"528","refunded":"0"}"#;

/// `InferenceOrderBook.InferenceOrderCancelRejected`, captured from event message
/// `a761aaf992289802a963bd8ce8e830eee159ccedbdc20494921b9cf56599f08d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f1`.
///
/// Captured 2026-08-08, exactly on the harvest window's boundary date and counted as
/// fresh; the sole rank-1 candidate of its type. Passed decode validation against its
/// captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_CANCEL_REJECTED: &str =
    "te6ccgEBAQEAOQAAbXwo6fcAAAAAAAAAAAAAAAAAAAAGAYACxDrV+QqgA5a/I+ef3w7HEju+IBgke+I3JQITOflKTNA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_CANCEL_REJECTED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_CANCEL_REJECTED_DECODED: &str = r#"{"note":"0:1621d6afc855001cb5f91f3cfef8763891ddf100c123df11b9281099cfca5266","reason":"1","orderId":"6"}"#;

/// `InferenceOrderBook.InferenceOrderExpired`, captured from event message
/// `83ecb128592fff0feb27a66069bdad70c88ebc68965694c5300aed9ae154ad1f` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f2`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_EXPIRED: &str = "te6ccgEBAgEAXQABax2o6lQAAAAAAAAAAAAAAAAAAAACwAjeBvUY/+DCFwjUVCRcmeCPMarxcYu0PCFNYDfxKRbw2AEAQ4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_EXPIRED`] at capture
/// time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_EXPIRED_DECODED: &str = r#"{"note":"0:8de06f518ffe0c21708d454245c99e08f31aaf1718bb43c214d6037f12916f0d","isBuy":true,"orderId":"2","tokenContract":"0:0000000000000000000000000000000000000000000000000000000000000000"}"#;

/// `InferenceOrderBook.InferenceRefunded`, captured from event message
/// `d6b8faf2ce003a53b6835e582092b0489e3ce36d916bb13e6de72300b468e119` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003ea`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_REFUNDED: &str = "te6ccgEBAQEASAAAizv8L8MAAAAAAAAAAAAAAAAAAAIRgBUAeKJUqULkEVwAYQ2+MI36Jt/ZHP02YXFWcN22U8nDoAAAAAAAAAAAAAAAAAAAABA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_REFUNDED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_REFUNDED_DECODED: &str = r#"{"note":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","amount":"0","orderId":"529"}"#;
