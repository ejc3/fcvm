//! The snapshot gate's removal, spoken to nf_tables directly over nfnetlink.
//!
//! The guest's `iptables` is the nf_tables backend, so the gate `close()`
//! installs with it lives in the `filter` table of the `ip` and `ip6`
//! families: two regular chains, and one jump rule in each of the `INPUT` and
//! `OUTPUT` base chains. Opening a half of the gate deletes, in both families,
//! that half's jump rules, then flushes and deletes its chain. This module
//! does that in ONE nf_tables transaction for both families: one batch, one
//! commit, no process spawn. `iptables-restore` can only commit one family per
//! process, so the same removal through it was two spawns and two commits per
//! half, and between the two commits one family was open and the other not.
//!
//! What it reads before the batch is the rule handles (one dump per base chain
//! per family) and the chain names (one dump per family). A rule handle names
//! one rule for the table's lifetime and is never reused, so a batch built
//! from those reads deletes exactly the rules it read or, if a rule has gone
//! since, fails as a whole: nf_tables aborts the transaction on the first
//! failing message and nothing is removed in either family.

use std::io;

use anyhow::{bail, Context, Result};

use crate::netlink::{
    append_netlink_header, decode_netlink_error, for_each_netlink_message, netlink_align,
    read_dump, NetlinkChannel, NetlinkHeader, OnInterrupt, NEXT_NETLINK_SEQUENCE, NLMSG_ERROR,
    NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST,
};

/// `NETLINK_NETFILTER` from linux/netlink.h.
pub(crate) const NETLINK_NETFILTER: i32 = 12;

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
const NFNL_MSG_BATCH_END: u16 = 0x11;
const NFNETLINK_V0: u8 = 0;

const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_GETCHAIN: u16 = 4;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_DELRULE: u16 = 8;

const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_IMMEDIATE_DREG: u16 = 1;
const NFTA_IMMEDIATE_DATA: u16 = 2;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_VERDICT_CHAIN: u16 = 2;

/// `NFT_REG_VERDICT`: an immediate that writes the verdict register.
const NFT_REG_VERDICT: u32 = 0;
/// `NFT_JUMP` from linux/netfilter/nf_tables.h.
const NFT_JUMP: i32 = -3;

/// The flags the kernel may set on an attribute type.
const NLA_TYPE_MASK: u16 = 0x3fff;

/// The table iptables-nft keeps its filter chains in, in both families.
const FILTER_TABLE: &str = "filter";

/// An nf_tables address family the gate lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Ipv4,
    Ipv6,
}

impl Family {
    pub(crate) const BOTH: [Family; 2] = [Family::Ipv4, Family::Ipv6];

    fn nfproto(self) -> u8 {
        match self {
            // NFPROTO_IPV4 and NFPROTO_IPV6.
            Family::Ipv4 => 2,
            Family::Ipv6 => 10,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Family::Ipv4 => "ip",
            Family::Ipv6 => "ip6",
        }
    }
}

/// One half of the gate as nf_tables holds it: the base chain the hook sits
/// in and the regular chain it jumps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GateHalf {
    pub(crate) hook: &'static str,
    pub(crate) chain: &'static str,
}

/// What one family's filter table holds of one gate half.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HalfLayout {
    /// Handles of the rules in the base chain that are exactly a jump to the
    /// gate chain, in chain order. A jump carrying any match is not a hook.
    pub(crate) hooks: Vec<u64>,
    /// Whether the gate chain exists.
    pub(crate) chain: bool,
}

/// Bounded: more duplicate hooks than any interrupted close could leave is
/// tampering, and deleting without bound would hold the restore instead of
/// reporting it.
const MAX_DUPLICATE_HOOKS: usize = 64;

fn nft_type(message: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | message
}

fn next_sequence() -> u32 {
    NEXT_NETLINK_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn append_attribute(bytes: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let length = 4 + payload.len();
    bytes.extend_from_slice(&(length as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize(netlink_align(bytes.len()), 0);
}

fn append_string(bytes: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = value.as_bytes().to_vec();
    payload.push(0);
    append_attribute(bytes, kind, &payload);
}

/// One nfnetlink message: header, `nfgenmsg`, attributes.
fn message(
    message_type: u16,
    flags: u16,
    sequence: u32,
    family: u8,
    res_id: u16,
    attributes: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20 + attributes.len());
    append_netlink_header(
        &mut bytes,
        NetlinkHeader {
            length: (16 + 4 + attributes.len()) as u32,
            message_type,
            flags,
            sequence,
            port_id: 0,
        },
    );
    bytes.push(family);
    bytes.push(NFNETLINK_V0);
    bytes.extend_from_slice(&res_id.to_be_bytes());
    bytes.extend_from_slice(attributes);
    bytes
}

/// Walk the attributes in `bytes`, yielding (type without flags, payload).
fn attributes(bytes: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut found = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= bytes.len() {
        let length = u16::from_ne_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        let kind = u16::from_ne_bytes([bytes[offset + 2], bytes[offset + 3]]) & NLA_TYPE_MASK;
        if length < 4 || offset + length > bytes.len() {
            bail!("invalid nfnetlink attribute length {length} at offset {offset}");
        }
        found.push((kind, &bytes[offset + 4..offset + length]));
        offset += netlink_align(length);
    }
    Ok(found)
}

fn attribute<'a>(list: &[(u16, &'a [u8])], kind: u16) -> Option<&'a [u8]> {
    list.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v)
}

fn string_attribute(payload: &[u8]) -> &[u8] {
    payload.split(|byte| *byte == 0).next().unwrap_or(payload)
}

fn be_u32(payload: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(payload.get(..4)?.try_into().ok()?))
}

/// Whether a rule's expressions are exactly a jump to `chain`, optionally
/// with the counter iptables-nft attaches to every rule.
fn rule_is_hook(expressions: &[u8], chain: &str) -> Result<bool> {
    let mut jump = false;
    for (kind, element) in attributes(expressions)? {
        if kind != NFTA_LIST_ELEM {
            return Ok(false);
        }
        let expression = attributes(element)?;
        let name = attribute(&expression, NFTA_EXPR_NAME).map(string_attribute);
        match name {
            Some(b"counter") => {}
            Some(b"immediate") if !jump => {
                let Some(data) = attribute(&expression, NFTA_EXPR_DATA) else {
                    return Ok(false);
                };
                let data = attributes(data)?;
                if attribute(&data, NFTA_IMMEDIATE_DREG).and_then(be_u32) != Some(NFT_REG_VERDICT) {
                    return Ok(false);
                }
                let Some(value) = attribute(&data, NFTA_IMMEDIATE_DATA) else {
                    return Ok(false);
                };
                let Some(verdict) = attribute(&attributes(value)?, NFTA_DATA_VERDICT) else {
                    return Ok(false);
                };
                let verdict = attributes(verdict)?;
                let code = attribute(&verdict, NFTA_VERDICT_CODE).and_then(be_u32);
                let target = attribute(&verdict, NFTA_VERDICT_CHAIN).map(string_attribute);
                if code != Some(NFT_JUMP as u32) || target != Some(chain.as_bytes()) {
                    return Ok(false);
                }
                jump = true;
            }
            _ => return Ok(false),
        }
    }
    Ok(jump)
}

fn dump<C: NetlinkChannel>(
    channel: &mut C,
    family: Family,
    request: u16,
    record: u16,
    attributes_bytes: &[u8],
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let sequence = next_sequence();
    let request_type = nft_type(request);
    let datagram = message(
        request_type,
        NLM_F_REQUEST | NLM_F_DUMP,
        sequence,
        family.nfproto(),
        0,
        attributes_bytes,
    );
    channel.send(&datagram)?;
    read_dump(
        channel,
        sequence,
        request_type,
        nft_type(record),
        OnInterrupt::Refuse,
        |_, payload| {
            // Skip the nfgenmsg in front of the attributes.
            let body = payload.get(4..).context("short nf_tables record")?;
            each(body)
        },
        |_, _| Ok(()),
    )?;
    Ok(())
}

/// Read what `family`'s filter table holds of `half`: the hook rules' handles
/// and whether the chain exists. A family with no filter table holds nothing.
pub(crate) fn read_half<C: NetlinkChannel>(
    channel: &mut C,
    family: Family,
    half: GateHalf,
) -> Result<HalfLayout> {
    let mut layout = HalfLayout::default();
    let mut request = Vec::new();
    append_string(&mut request, NFTA_CHAIN_TABLE, FILTER_TABLE);
    dump(
        channel,
        family,
        NFT_MSG_GETCHAIN,
        NFT_MSG_NEWCHAIN,
        &request,
        |body| {
            let list = attributes(body)?;
            if attribute(&list, NFTA_CHAIN_TABLE).map(string_attribute)
                == Some(FILTER_TABLE.as_bytes())
                && attribute(&list, NFTA_CHAIN_NAME).map(string_attribute)
                    == Some(half.chain.as_bytes())
            {
                layout.chain = true;
            }
            Ok(())
        },
    )
    .with_context(|| format!("listing the {} filter chains", family.name()))?;
    let mut request = Vec::new();
    append_string(&mut request, NFTA_RULE_TABLE, FILTER_TABLE);
    append_string(&mut request, NFTA_RULE_CHAIN, half.hook);
    dump(
        channel,
        family,
        NFT_MSG_GETRULE,
        NFT_MSG_NEWRULE,
        &request,
        |body| {
            let list = attributes(body)?;
            if attribute(&list, NFTA_RULE_TABLE).map(string_attribute)
                != Some(FILTER_TABLE.as_bytes())
                || attribute(&list, NFTA_RULE_CHAIN).map(string_attribute)
                    != Some(half.hook.as_bytes())
            {
                return Ok(());
            }
            let handle = attribute(&list, NFTA_RULE_HANDLE)
                .and_then(|value| Some(u64::from_be_bytes(value.get(..8)?.try_into().ok()?)))
                .context("an nf_tables rule without a handle")?;
            if let Some(expressions) = attribute(&list, NFTA_RULE_EXPRESSIONS) {
                if rule_is_hook(expressions, half.chain)? {
                    layout.hooks.push(handle);
                }
            }
            Ok(())
        },
    )
    .with_context(|| format!("listing the {} filter {} rules", family.name(), half.hook))?;
    if layout.hooks.len() > MAX_DUPLICATE_HOOKS {
        bail!(
            "{} {} jumps to {} {} times (limit {MAX_DUPLICATE_HOOKS}); refusing cleanup",
            family.name(),
            half.hook,
            half.chain,
            layout.hooks.len()
        );
    }
    Ok(layout)
}

/// One planned removal: a family, a half, and what was read of it.
pub(crate) struct Removal {
    pub(crate) family: Family,
    pub(crate) half: GateHalf,
    pub(crate) layout: HalfLayout,
}

/// The batch that removes every `removals` entry, and the sequence numbers of
/// its BEGIN and END messages. Per entry: the hook rules by handle, then the
/// chain's flush and deletion, the order iptables-restore uses. Returns `None`
/// when there is nothing to remove.
pub(crate) fn removal_batch(removals: &[Removal]) -> Option<(Vec<u8>, u32, u32)> {
    let mut body = Vec::new();
    let mut messages = 0usize;
    for removal in removals {
        let family = removal.family.nfproto();
        for handle in &removal.layout.hooks {
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_RULE_CHAIN, removal.half.hook);
            append_attribute(&mut attrs, NFTA_RULE_HANDLE, &handle.to_be_bytes());
            body.extend(message(
                nft_type(NFT_MSG_DELRULE),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            messages += 1;
        }
        if removal.layout.chain {
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_RULE_CHAIN, removal.half.chain);
            body.extend(message(
                nft_type(NFT_MSG_DELRULE),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            let mut attrs = Vec::new();
            append_string(&mut attrs, NFTA_CHAIN_TABLE, FILTER_TABLE);
            append_string(&mut attrs, NFTA_CHAIN_NAME, removal.half.chain);
            body.extend(message(
                nft_type(NFT_MSG_DELCHAIN),
                NLM_F_REQUEST,
                next_sequence(),
                family,
                0,
                &attrs,
            ));
            messages += 2;
        }
    }
    if messages == 0 {
        return None;
    }
    let begin = next_sequence();
    let mut batch = message(
        NFNL_MSG_BATCH_BEGIN,
        NLM_F_REQUEST,
        begin,
        0,
        NFNL_SUBSYS_NFTABLES,
        &[],
    );
    batch.extend(body);
    let end = next_sequence();
    batch.extend(message(
        NFNL_MSG_BATCH_END,
        NLM_F_REQUEST | NLM_F_ACK,
        end,
        0,
        NFNL_SUBSYS_NFTABLES,
        &[],
    ));
    Some((batch, begin, end))
}

/// Send a batch and wait for its verdict. nf_tables answers a committed batch
/// with an acknowledgement of its END message (the only message that asks for
/// one), and a failed one with an error naming the message that failed or,
/// for a failed commit, the BEGIN message; a failed batch changes nothing.
pub(crate) fn commit<C: NetlinkChannel>(channel: &mut C, batch: &[u8], end: u32) -> Result<()> {
    channel.send(batch).context("sending the nf_tables batch")?;
    loop {
        let datagram = channel
            .receive()
            .context("waiting for the nf_tables batch verdict")?;
        let mut verdict: Option<Result<()>> = None;
        for_each_netlink_message(datagram, |header, payload| {
            if header.message_type != NLMSG_ERROR {
                return Ok(false);
            }
            let (error, request) = decode_netlink_error(payload)?;
            if error != 0 {
                let errno = error
                    .checked_neg()
                    .filter(|errno| *errno > 0)
                    .unwrap_or(libc::EIO);
                verdict = Some(Err(anyhow::anyhow!(
                    "nf_tables refused the batch at message type {:#x} sequence {}: {}",
                    request.message_type,
                    request.sequence,
                    io::Error::from_raw_os_error(errno)
                )));
                return Ok(true);
            }
            if request.sequence == end && request.message_type == NFNL_MSG_BATCH_END {
                verdict = Some(Ok(()));
                return Ok(true);
            }
            Ok(false)
        })?;
        if let Some(verdict) = verdict {
            return verdict;
        }
    }
}

#[cfg(test)]
pub(crate) mod model {
    //! A model of nf_tables' filter tables in both families, speaking the
    //! same nfnetlink messages as the kernel: chain and rule dumps, and
    //! batches that apply whole or not at all.

    use super::*;
    use crate::netlink::NLMSG_DONE;
    use std::collections::VecDeque;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Rule {
        /// `-j <chain>` with iptables-nft's counter.
        Jump(&'static str),
        /// Anything else, e.g. a jump that carries a match.
        Other,
    }

    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(crate) struct Table {
        pub(crate) chains: Vec<&'static str>,
        /// (chain, handle, rule)
        pub(crate) rules: Vec<(&'static str, u64, Rule)>,
    }

    #[derive(Debug, Default)]
    pub(crate) struct Kernel {
        pub(crate) ipv4: Table,
        pub(crate) ipv6: Table,
        /// Every committed batch, as the list of (family, operation) it applied.
        pub(crate) commits: Vec<Vec<String>>,
        pub(crate) refused: usize,
        /// While set, closing a channel waits for a message on it (up to two
        /// seconds), as a real close waits out an RCU grace period.
        pub(crate) close_blocker:
            Option<std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>>,
        /// Channels opened and closed so far.
        pub(crate) opens: usize,
        pub(crate) closes: usize,
        next_handle: u64,
        pending: VecDeque<Vec<u8>>,
    }

    impl Kernel {
        pub(crate) fn table(&mut self, family: u8) -> &mut Table {
            if family == 2 {
                &mut self.ipv4
            } else {
                &mut self.ipv6
            }
        }

        /// The armed gate in one family, as iptables-nft lays it out.
        pub(crate) fn arm(&mut self, family: Family) {
            let handles = [
                self.next_handle + 1,
                self.next_handle + 2,
                self.next_handle + 3,
            ];
            self.next_handle += 3;
            let table = self.table(family.nfproto());
            table.chains = vec![
                "INPUT",
                "FORWARD",
                "OUTPUT",
                "FCVM_SNAPSHOT_IN",
                "FCVM_SNAPSHOT_OUT",
            ];
            table.rules = vec![
                ("INPUT", handles[0], Rule::Jump("FCVM_SNAPSHOT_IN")),
                ("OUTPUT", handles[1], Rule::Jump("FCVM_SNAPSHOT_OUT")),
                ("FCVM_SNAPSHOT_IN", handles[2], Rule::Other),
            ];
        }

        pub(crate) fn add_rule(&mut self, family: Family, chain: &'static str, rule: Rule) -> u64 {
            self.next_handle += 1;
            let handle = self.next_handle;
            self.table(family.nfproto())
                .rules
                .push((chain, handle, rule));
            handle
        }
    }

    fn rule_record(family: u8, chain: &str, handle: u64, rule: &Rule, sequence: u32) -> Vec<u8> {
        let mut attrs = Vec::new();
        append_string(&mut attrs, NFTA_RULE_TABLE, FILTER_TABLE);
        append_string(&mut attrs, NFTA_RULE_CHAIN, chain);
        append_attribute(&mut attrs, NFTA_RULE_HANDLE, &handle.to_be_bytes());
        let mut list = Vec::new();
        let mut counter = Vec::new();
        append_string(&mut counter, NFTA_EXPR_NAME, "counter");
        append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &counter);
        if let Rule::Jump(target) = rule {
            let mut verdict = Vec::new();
            append_attribute(
                &mut verdict,
                NFTA_VERDICT_CODE,
                &(NFT_JUMP as u32).to_be_bytes(),
            );
            append_string(&mut verdict, NFTA_VERDICT_CHAIN, target);
            let mut value = Vec::new();
            append_attribute(&mut value, NFTA_DATA_VERDICT | 0x8000, &verdict);
            let mut data = Vec::new();
            append_attribute(
                &mut data,
                NFTA_IMMEDIATE_DREG,
                &NFT_REG_VERDICT.to_be_bytes(),
            );
            append_attribute(&mut data, NFTA_IMMEDIATE_DATA | 0x8000, &value);
            let mut expression = Vec::new();
            append_string(&mut expression, NFTA_EXPR_NAME, "immediate");
            append_attribute(&mut expression, NFTA_EXPR_DATA | 0x8000, &data);
            append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &expression);
        } else {
            let mut expression = Vec::new();
            append_string(&mut expression, NFTA_EXPR_NAME, "match");
            append_attribute(&mut list, NFTA_LIST_ELEM | 0x8000, &expression);
        }
        append_attribute(&mut attrs, NFTA_RULE_EXPRESSIONS | 0x8000, &list);
        message(nft_type(NFT_MSG_NEWRULE), 0x2, sequence, family, 0, &attrs)
    }

    fn done(sequence: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: 20,
                message_type: NLMSG_DONE,
                flags: 0x2,
                sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(&0i32.to_ne_bytes());
        bytes
    }

    fn error(request: NetlinkHeader, errno: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_netlink_header(
            &mut bytes,
            NetlinkHeader {
                length: 36,
                message_type: NLMSG_ERROR,
                flags: 0,
                sequence: request.sequence,
                port_id: 0,
            },
        );
        bytes.extend_from_slice(&(-errno).to_ne_bytes());
        append_netlink_header(&mut bytes, request);
        bytes
    }

    pub(crate) struct Channel {
        pub(crate) kernel: std::sync::Arc<std::sync::Mutex<Kernel>>,
        buffer: Vec<u8>,
    }

    impl Channel {
        pub(crate) fn new(kernel: std::sync::Arc<std::sync::Mutex<Kernel>>) -> Self {
            Self {
                kernel,
                buffer: Vec::new(),
            }
        }
    }

    impl Drop for Channel {
        fn drop(&mut self) {
            let blocker = self.kernel.lock().unwrap().close_blocker.clone();
            if let Some(blocker) = blocker {
                let _ = blocker
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(2));
            }
            self.kernel.lock().unwrap().closes += 1;
        }
    }

    impl NetlinkChannel for Channel {
        fn send(&mut self, datagram: &[u8]) -> Result<()> {
            let mut kernel = self.kernel.lock().unwrap();
            let mut messages = Vec::new();
            for_each_netlink_message(datagram, |header, payload| {
                messages.push((header, payload.to_vec()));
                Ok(false)
            })?;
            let (first, _) = messages[0];
            if first.message_type == NFNL_MSG_BATCH_BEGIN {
                let mut working = (kernel.ipv4.clone(), kernel.ipv6.clone());
                let mut applied = Vec::new();
                let mut failure = None;
                for (header, payload) in &messages[1..] {
                    if header.message_type == NFNL_MSG_BATCH_END {
                        break;
                    }
                    let family = payload[0];
                    let table = if family == 2 {
                        &mut working.0
                    } else {
                        &mut working.1
                    };
                    let list = attributes(&payload[4..])?;
                    let name = |kind| {
                        String::from_utf8(
                            string_attribute(attribute(&list, kind).unwrap()).to_vec(),
                        )
                        .unwrap()
                    };
                    let label = if family == 2 { "ip" } else { "ip6" };
                    let result: std::result::Result<String, i32> = match header.message_type & 0xff
                    {
                        NFT_MSG_DELRULE => {
                            let chain = name(NFTA_RULE_CHAIN);
                            match attribute(&list, NFTA_RULE_HANDLE) {
                                Some(value) => {
                                    let handle = u64::from_be_bytes(value.try_into().unwrap());
                                    match table
                                        .rules
                                        .iter()
                                        .position(|(c, h, _)| *c == chain && *h == handle)
                                    {
                                        Some(index) => {
                                            table.rules.remove(index);
                                            Ok(format!("{label} delete rule {chain} handle"))
                                        }
                                        None => Err(libc::ENOENT),
                                    }
                                }
                                None => {
                                    table.rules.retain(|(c, _, _)| *c != chain);
                                    Ok(format!("{label} flush chain {chain}"))
                                }
                            }
                        }
                        NFT_MSG_DELCHAIN => {
                            let chain = name(NFTA_CHAIN_NAME);
                            let referenced = table
                                .rules
                                .iter()
                                .any(|(_, _, r)| matches!(r, Rule::Jump(t) if *t == chain));
                            match table.chains.iter().position(|c| *c == chain) {
                                Some(_) if referenced => Err(libc::EBUSY),
                                Some(index) => {
                                    table.chains.remove(index);
                                    Ok(format!("{label} delete chain {chain}"))
                                }
                                None => Err(libc::ENOENT),
                            }
                        }
                        _ => Err(libc::EINVAL),
                    };
                    match result {
                        Ok(op) => applied.push(op),
                        Err(errno) => {
                            failure = Some(error(*header, errno));
                            break;
                        }
                    }
                }
                match failure {
                    Some(reply) => {
                        kernel.refused += 1;
                        kernel.pending.push_back(reply);
                    }
                    None => {
                        kernel.ipv4 = working.0;
                        kernel.ipv6 = working.1;
                        kernel.commits.push(applied);
                        let (end, _) = messages.last().unwrap();
                        kernel.pending.push_back(error(*end, 0));
                    }
                }
                return Ok(());
            }
            let family = messages[0].1[0];
            let list = attributes(&messages[0].1[4..])?;
            let mut reply = Vec::new();
            match first.message_type & 0xff {
                NFT_MSG_GETCHAIN => {
                    let chains = kernel.table(family).chains.clone();
                    for chain in chains {
                        let mut attrs = Vec::new();
                        append_string(&mut attrs, NFTA_CHAIN_TABLE, FILTER_TABLE);
                        append_string(&mut attrs, NFTA_CHAIN_NAME, chain);
                        reply.extend(message(
                            nft_type(NFT_MSG_NEWCHAIN),
                            0x2,
                            first.sequence,
                            family,
                            0,
                            &attrs,
                        ));
                    }
                }
                NFT_MSG_GETRULE => {
                    let wanted = String::from_utf8(
                        string_attribute(attribute(&list, NFTA_RULE_CHAIN).unwrap()).to_vec(),
                    )
                    .unwrap();
                    let rules = kernel.table(family).rules.clone();
                    for (chain, handle, rule) in rules.iter().filter(|(c, _, _)| *c == wanted) {
                        reply.extend(rule_record(family, chain, *handle, rule, first.sequence));
                    }
                }
                other => bail!("model kernel: unexpected request {other}"),
            }
            reply.extend(done(first.sequence));
            kernel.pending.push_back(reply);
            Ok(())
        }

        fn receive(&mut self) -> Result<&[u8]> {
            let next = self.kernel.lock().unwrap().pending.pop_front();
            match next {
                Some(datagram) => {
                    self.buffer = datagram;
                    Ok(&self.buffer)
                }
                None => bail!("model kernel: receive with nothing queued"),
            }
        }

        fn reserve_receive_buffer(&mut self, bytes: usize) -> Result<usize> {
            Ok(bytes)
        }
    }
}
