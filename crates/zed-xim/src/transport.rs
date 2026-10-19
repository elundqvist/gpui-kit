// Added in elundqvist/gpui-kit to zed-xim 0.4.0-zed as published to crates.io: FORK.md at the
// crate's root lists the changes.
//! How a request goes to the other side over X: in the 20 bytes of one
//! ClientMessage, or in a property of the other side's window, which a
//! ClientMessage names with its length.

/// The bytes a ClientMessage of format 8 holds.
pub(crate) const CM_DATA: usize = 20;

/// How the `bytes` of a request go, to a side that announced
/// `transport_max` with its XCONNECT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// In one ClientMessage, the request and zeros after it.
    ClientMessage([u8; CM_DATA]),
    /// In a property, which a ClientMessage of format 32 names.
    Property,
}

/// A request goes in a ClientMessage where it fits in one and is under
/// the other side's limit, and otherwise in a property. A server's
/// TRANSPORT_MAX is the most it takes in ClientMessages, a request
/// longer than one going in several, the protocol's multi-CM; one that
/// announced more than 20 had a request between 21 bytes and its limit
/// end the client on `todo!("multi-CM")`. A property is what a request
/// at or over the limit already went by, and IMdkit and xcb-imdkit,
/// which ibus and fcitx build their XIM servers on, read a property for
/// any request whatever limit they announced.
pub(crate) fn transport(bytes: &[u8], transport_max: usize) -> Transport {
    if bytes.len() <= CM_DATA && bytes.len() < transport_max {
        let mut data = [0u8; CM_DATA];
        data[..bytes.len()].copy_from_slice(bytes);
        Transport::ClientMessage(data)
    } else {
        Transport::Property
    }
}

#[cfg(test)]
mod tests {
    use super::{transport, Transport};

    #[test]
    fn a_request_goes_in_a_client_message_only_where_it_fits_in_one() {
        // the request and zeros after it
        let mut eight = [0u8; 20];
        eight[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(transport(&[1, 2, 3, 4, 5, 6, 7, 8], 20), Transport::ClientMessage(eight));
        assert_eq!(transport(&[7; 19], 20), Transport::ClientMessage({
            let mut d = [7u8; 20];
            d[19] = 0;
            d
        }));
        // at the limit, and over it
        assert_eq!(transport(&[7; 20], 20), Transport::Property);
        assert_eq!(transport(&[7; 400], 20), Transport::Property);
        // a server with a limit of 0 takes everything by property
        assert_eq!(transport(&[7; 8], 0), Transport::Property);
        // under a large limit: 20 bytes fill one message, and the 21st
        // and on go by property, as at the limit
        assert_eq!(transport(&[7; 20], 4096), Transport::ClientMessage([7; 20]));
        for len in [21, 28, 100, 4095, 4096, 9000] {
            assert_eq!(transport(&alloc::vec![7; len], 4096), Transport::Property, "{len} bytes");
        }
    }
}
