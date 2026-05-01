//! Pure ABI decode helpers used by the verbose printers.
//!
//! These are split from [`super::etherscan`] (which only owns the HTTP cache)
//! and from [`super::print`] (which owns the formatting) so they can be
//! tested in isolation and so the verbose entry-points read as a thin layer
//! over `decode_*` calls.

use alloy_dyn_abi::{DynSolType, DynSolValue, JsonAbiExt};
use alloy_json_abi::JsonAbi;
use alloy_primitives::Bytes;

/// Render a [`DynSolValue`] as a short string suitable for one-line printing.
/// Long byte arrays are truncated with a `…(len=N)` marker.
pub fn render_value(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Address(a) => format!("0x{:x}", a),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::FixedBytes(b, n) => format!("0x{}", hex::encode(&b.0[..*n])),
        DynSolValue::Bytes(b) => {
            if b.len() > 80 {
                format!("0x{}…(len={})", hex::encode(&b[..40]), b.len())
            } else {
                format!("0x{}", hex::encode(b))
            }
        }
        DynSolValue::String(s) => format!("{s:?}"),
        DynSolValue::Array(arr) | DynSolValue::FixedArray(arr) => {
            let inner: Vec<_> = arr.iter().map(render_value).collect();
            format!("[{}]", inner.join(", "))
        }
        DynSolValue::Tuple(t) => {
            let inner: Vec<_> = t.iter().map(render_value).collect();
            format!("({})", inner.join(", "))
        }
        _ => format!("{:?}", v),
    }
}

/// Decoded view of a successful function-call match against an ABI.
pub struct DecodedCall {
    pub name: String,
    pub signature: String,
    /// `(typed_name, rendered_value)` for each input parameter.
    pub args: Vec<(String, String)>,
}

/// Decode `calldata` against `abi`, looking for a function whose 4-byte
/// selector matches and whose input schema decodes the rest of the data.
/// Returns `None` if no function matches.
pub fn decode_call(abi: &JsonAbi, calldata: &Bytes) -> Option<DecodedCall> {
    if calldata.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = calldata[..4].try_into().ok()?;
    let body = &calldata[4..];

    for func in abi.functions() {
        if func.selector().0 != selector {
            continue;
        }
        let Ok(decoded) = func.abi_decode_input(body) else {
            continue;
        };
        let args: Vec<(String, String)> = func
            .inputs
            .iter()
            .zip(decoded.iter())
            .map(|(p, v)| {
                let name = if p.name.is_empty() {
                    p.ty.clone()
                } else {
                    format!("{} {}", p.ty, p.name)
                };
                (name, render_value(v))
            })
            .collect();
        return Some(DecodedCall {
            name: func.name.clone(),
            signature: func.signature(),
            args,
        });
    }
    None
}

/// Decode revert `data` into a human-readable string. Handles the standard
/// `Error(string)` and `Panic(uint256)` selectors, and any custom error
/// declared in `abi` (when provided).
pub fn decode_revert(abi: Option<&JsonAbi>, data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }

    if data[..4] == [0x08, 0xc3, 0x79, 0xa0]
        && let Ok(DynSolValue::String(s)) = DynSolType::String.abi_decode(&data[4..])
    {
        return Some(format!("Error({s:?})"));
    }

    if data[..4] == [0x4e, 0x48, 0x7b, 0x71]
        && let Ok(DynSolValue::Uint(code, _)) = DynSolType::Uint(256).abi_decode(&data[4..])
    {
        return Some(format!("Panic(0x{:x})", code));
    }

    if let Some(abi) = abi {
        let selector: [u8; 4] = data[..4].try_into().ok()?;
        for err in abi.errors() {
            if err.selector().0 == selector
                && let Ok(decoded) = err.abi_decode_input(&data[4..])
            {
                let inner: Vec<String> = decoded.iter().map(render_value).collect();
                return Some(format!("{}({})", err.name, inner.join(", ")));
            }
        }
    }

    None
}
