// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the ai:inference streaming end-to-end proof.
//!
//! Built against the `inference-guest` world (../wit): it imports
//! `hellohq:plugin/inference` (and transitively `hellohq:plugin/types`) and
//! exports `run: async func() -> list<u8>`. `run` calls
//! `inference.complete([{role:"user", content:"hello"}], {max-tokens:64})`,
//! drains the returned `stream<string>` concatenating the token deltas, and
//! returns the concatenation as bytes — so the host test can assert streamed
//! token deltas round-trip through the full inference resource + stream +
//! concurrent-complete path.
//!
//! `complete` is a SYNC WIT func that the host binds `store` (so it can mint the
//! `stream<string>`); a guest may still lower a concurrent host import
//! synchronously, so `complete` is called blocking. `run` is `async func`
//! because it drains the returned stream (which yields), so the export task must
//! be async.

wit_bindgen::generate!({
    path: "../wit",
    world: "inference-guest",
    // Generate the transitively-used `hellohq:plugin/types` interface inline
    // rather than requiring `with` mappings.
    generate_all,
});

use hellohq::plugin::inference::{complete, ChatMessage, InferenceOpts};

struct Component;

impl Guest for Component {
    // `run` is exported async because it drains the returned `stream<string>`
    // (which yields).
    async fn run() -> Vec<u8> {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];
        let opts = InferenceOpts {
            max_tokens: 64,
            temperature: None,
        };

        // Call the host completion. `complete` mints a `stream<string>` of token
        // deltas; on `Err` (gate denial / parse failure) return empty.
        let token_stream = match complete(&messages, opts) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        // Drain the stream, concatenating each token-delta string.
        let tokens: Vec<String> = token_stream.collect().await;
        let mut text = String::new();
        for t in tokens {
            text.push_str(&t);
        }
        text.into_bytes()
    }
}

export!(Component);
