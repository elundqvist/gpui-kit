// Changed in elundqvist/gpui-kit from zed-xim 0.4.0-zed as published to crates.io: FORK.md at the
// crate's root lists the changes.
mod attribute_builder;

pub use self::attribute_builder::AttributeBuilder;
use crate::AHashMap;
use xim_parser::{
    Attr, Attribute, AttributeName, CaretDirection, CaretStyle, CommitData, Extension, Feedback,
    ForwardEventFlag, PreeditDrawStatus, Request,
};

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    ReadProtocol(xim_parser::ReadError),
    XimError(xim_parser::ErrorCode, String),
    UnsupportedTransport,
    InvalidReply,
    NoXimServer,
    #[cfg(feature = "std")]
    Other(alloc::boxed::Box<dyn std::error::Error + Send + Sync>),
}

impl From<xim_parser::ReadError> for ClientError {
    fn from(e: xim_parser::ReadError) -> Self {
        Self::ReadProtocol(e)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::ReadProtocol(e) => write!(f, "Can't read xim message: {}", e),
            ClientError::XimError(code, detail) => {
                write!(f, "Server send error code: {:?}, detail: {}", code, detail)
            }
            ClientError::UnsupportedTransport => write!(f, "Server Transport is not supported"),
            ClientError::InvalidReply => write!(f, "Invalid reply from server"),
            ClientError::NoXimServer => write!(f, "Can't connect xim server"),
            #[cfg(feature = "std")]
            ClientError::Other(e) => write!(f, "Other error: {}", e),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ClientError {}

pub fn handle_request<C: ClientCore>(
    client: &mut C,
    handler: &mut impl ClientHandler<C>,
    req: Request,
) -> Result<(), ClientError> {
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("<-: {:?}", req);
    } else {
        log::debug!("<-: {}", req.name());
    }

    match req {
        Request::ConnectReply {
            server_major_protocol_version: _,
            server_minor_protocol_version: _,
        } => handler.handle_connect(client),
        Request::OpenReply {
            input_method_id,
            im_attrs,
            ic_attrs,
        } => {
            log::debug!("im_attrs: {:#?}", im_attrs);
            log::debug!("ic_attrs: {:#?}", ic_attrs);
            client.set_attrs(im_attrs, ic_attrs);
            // Require for uim
            client.send_req(Request::EncodingNegotiation {
                encodings: vec!["COMPOUND_TEXT".into()],
                encoding_infos: vec![],
                input_method_id,
            })
        }
        Request::EncodingNegotiationReply {
            input_method_id,
            index: _,
            category: _,
        } => handler.handle_open(client, input_method_id),
        Request::QueryExtensionReply {
            input_method_id: _,
            extensions,
        } => handler.handle_query_extension(client, &extensions),
        Request::GetImValuesReply {
            input_method_id,
            im_attributes,
        } => handler.handle_get_im_values(
            client,
            input_method_id,
            im_attributes
                .into_iter()
                .filter_map(|attr| {
                    client
                        .im_attributes()
                        .iter()
                        .find(|(_, v)| **v == attr.id)
                        .map(|(n, _)| (*n, attr.value))
                })
                .collect(),
        ),
        Request::SetIcValuesReply {
            input_method_id,
            input_context_id,
        } => handler.handle_set_ic_values(client, input_method_id, input_context_id),
        Request::CreateIcReply {
            input_method_id,
            input_context_id,
        } => handler.handle_create_ic(client, input_method_id, input_context_id),
        Request::DestroyIcReply {
            input_method_id,
            input_context_id,
        } => handler.handle_destroy_ic(client, input_method_id, input_context_id),
        Request::SetEventMask {
            input_method_id,
            input_context_id,
            forward_event_mask,
            synchronous_event_mask,
        } => handler.handle_set_event_mask(
            client,
            input_method_id,
            input_context_id,
            forward_event_mask,
            synchronous_event_mask,
        ),
        Request::CloseReply { input_method_id } => handler.handle_close(client, input_method_id),
        Request::DisconnectReply {} => {
            handler.handle_disconnect();
            Ok(())
        }
        Request::ResetIcReply {
            input_method_id,
            input_context_id,
            preedit_string,
        } => {
            let preedit_string = crate::ctext::compound_text_to_utf8_lossy(&preedit_string);
            handler.handle_reset_ic(client, input_method_id, input_context_id, &preedit_string)
        }
        Request::Error { code, detail, .. } => Err(ClientError::XimError(code, detail)),
        Request::ForwardEvent {
            xev,
            input_method_id,
            input_context_id,
            flag,
            ..
        } => {
            handler.handle_forward_event(
                client,
                input_method_id,
                input_context_id,
                flag,
                client.deserialize_event(&xev),
            )?;

            if flag.contains(ForwardEventFlag::SYNCHRONOUS) {
                client.send_req(Request::SyncReply {
                    input_method_id,
                    input_context_id,
                })?;
            }

            Ok(())
        }
        Request::Commit {
            input_method_id,
            input_context_id,
            data,
        } => match data {
            CommitData::Keysym { keysym: _, .. } => {
                log::warn!("Keysym commit is not supported");
                Ok(())
            }
            CommitData::Chars {
                commited,
                syncronous,
            } => {
                handler.handle_commit(
                    client,
                    input_method_id,
                    input_context_id,
                    &crate::ctext::compound_text_to_utf8_lossy(&commited),
                )?;

                if syncronous {
                    client.send_req(Request::SyncReply {
                        input_method_id,
                        input_context_id,
                    })?;
                }

                Ok(())
            }
            CommitData::Both { .. } => {
                log::warn!("Both commit data is not supported");
                Ok(())
            }
        },
        Request::Sync {
            input_method_id,
            input_context_id,
        } => client.send_req(Request::SyncReply {
            input_method_id,
            input_context_id,
        }),
        Request::SyncReply { .. } => {
            // Nothing to do
            Ok(())
        }
        Request::PreeditStart {
            input_method_id,
            input_context_id,
        } => handler.handle_preedit_start(client, input_method_id, input_context_id),
        Request::PreeditDone {
            input_method_id,
            input_context_id,
        } => handler.handle_preedit_done(client, input_method_id, input_context_id),
        Request::PreeditDraw {
            input_method_id,
            input_context_id,
            caret,
            chg_first,
            chg_length,
            preedit_string,
            status,
            feedbacks,
        } => {
            let preedit_string = crate::ctext::compound_text_to_utf8_lossy(&preedit_string);
            handler.handle_preedit_draw(
                client,
                input_method_id,
                input_context_id,
                caret,
                chg_first,
                chg_length,
                status,
                &preedit_string,
                feedbacks,
            )
        }
        Request::PreeditCaret {
            input_method_id,
            input_context_id,
            mut position,
            direction,
            style,
        } => {
            // Handle the request.
            handler.handle_preedit_caret(
                client,
                input_method_id,
                input_context_id,
                &mut position,
                direction,
                style,
            )?;

            // Send the reply.
            client.send_req(Request::PreeditCaretReply {
                input_method_id,
                input_context_id,
                position,
            })
        }
        _ => {
            log::warn!("Unknown request {:?}", req);
            Ok(())
        }
    }
}

pub trait ClientCore {
    type XEvent;

    fn set_attrs(&mut self, ic_attrs: Vec<Attr>, im_attrs: Vec<Attr>);
    fn ic_attributes(&self) -> &AHashMap<AttributeName, u16>;
    fn im_attributes(&self) -> &AHashMap<AttributeName, u16>;
    fn serialize_event(&self, xev: &Self::XEvent) -> xim_parser::XEvent;
    fn deserialize_event(&self, xev: &xim_parser::XEvent) -> Self::XEvent;
    fn send_req(&mut self, req: Request) -> Result<(), ClientError>;
}

pub trait Client {
    type XEvent;

    fn build_ic_attributes(&self) -> AttributeBuilder<'_>;
    fn build_im_attributes(&self) -> AttributeBuilder<'_>;

    fn disconnect(&mut self) -> Result<(), ClientError>;
    fn open(&mut self, locale: &str) -> Result<(), ClientError>;
    fn close(&mut self, input_method_id: u16) -> Result<(), ClientError>;
    fn quert_extension(
        &mut self,
        input_method_id: u16,
        extensions: &[&str],
    ) -> Result<(), ClientError>;
    fn get_im_values(
        &mut self,
        input_method_id: u16,
        names: &[AttributeName],
    ) -> Result<(), ClientError>;
    fn set_ic_values(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
        ic_attributes: Vec<Attribute>,
    ) -> Result<(), ClientError>;
    fn create_ic(
        &mut self,
        input_method_id: u16,
        ic_attributes: Vec<Attribute>,
    ) -> Result<(), ClientError>;
    fn destroy_ic(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError>;
    fn forward_event(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
        flag: ForwardEventFlag,
        xev: &Self::XEvent,
    ) -> Result<(), ClientError>;
    fn set_focus(&mut self, input_method_id: u16, input_context_id: u16)
        -> Result<(), ClientError>;
    fn unset_focus(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError>;
    fn reset_ic(&mut self, input_method_id: u16, input_context_id: u16) -> Result<(), ClientError>;
}

impl<C> Client for C
where
    C: ClientCore,
{
    type XEvent = C::XEvent;

    fn build_ic_attributes(&self) -> AttributeBuilder<'_> {
        AttributeBuilder::new(self.ic_attributes())
    }

    fn build_im_attributes(&self) -> AttributeBuilder<'_> {
        AttributeBuilder::new(self.im_attributes())
    }

    fn open(&mut self, locale: &str) -> Result<(), ClientError> {
        self.send_req(Request::Open {
            locale: locale.into(),
        })
    }

    fn quert_extension(
        &mut self,
        input_method_id: u16,
        extensions: &[&str],
    ) -> Result<(), ClientError> {
        self.send_req(Request::QueryExtension {
            input_method_id,
            extensions: extensions.iter().map(|&e| e.into()).collect(),
        })
    }

    fn get_im_values(
        &mut self,
        input_method_id: u16,
        names: &[AttributeName],
    ) -> Result<(), ClientError> {
        self.send_req(Request::GetImValues {
            input_method_id,
            im_attributes: names
                .iter()
                .filter_map(|name| self.im_attributes().get(name).copied())
                .collect(),
        })
    }

    fn set_ic_values(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
        ic_attributes: Vec<Attribute>,
    ) -> Result<(), ClientError> {
        self.send_req(Request::SetIcValues {
            input_method_id,
            input_context_id,
            ic_attributes,
        })
    }

    fn create_ic(
        &mut self,
        input_method_id: u16,
        ic_attributes: Vec<Attribute>,
    ) -> Result<(), ClientError> {
        self.send_req(Request::CreateIc {
            input_method_id,
            ic_attributes,
        })
    }

    fn forward_event(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
        flag: ForwardEventFlag,
        xev: &Self::XEvent,
    ) -> Result<(), ClientError> {
        let ev = self.serialize_event(xev);
        self.send_req(Request::ForwardEvent {
            input_method_id,
            input_context_id,
            flag,
            serial_number: ev.sequence,
            xev: ev,
        })
    }

    fn disconnect(&mut self) -> Result<(), ClientError> {
        self.send_req(Request::Disconnect {})
    }

    fn close(&mut self, input_method_id: u16) -> Result<(), ClientError> {
        self.send_req(Request::Close { input_method_id })
    }

    fn destroy_ic(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.send_req(Request::DestroyIc {
            input_method_id,
            input_context_id,
        })
    }

    fn set_focus(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.send_req(Request::SetIcFocus {
            input_method_id,
            input_context_id,
        })
    }
    fn unset_focus(
        &mut self,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.send_req(Request::UnsetIcFocus {
            input_method_id,
            input_context_id,
        })
    }
    fn reset_ic(&mut self, input_method_id: u16, input_context_id: u16) -> Result<(), ClientError> {
        self.send_req(Request::ResetIc {
            input_method_id,
            input_context_id,
        })
    }
}

#[allow(unused_variables)]
pub trait ClientHandler<C: Client> {
    fn handle_connect(&mut self, client: &mut C) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_disconnect(&mut self) {}
    fn handle_open(&mut self, client: &mut C, input_method_id: u16) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_close(&mut self, client: &mut C, input_method_id: u16) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_query_extension(
        &mut self,
        client: &mut C,
        extensions: &[Extension],
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_get_im_values(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        attributes: AHashMap<AttributeName, Vec<u8>>,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_set_ic_values(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_create_ic(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_destroy_ic(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_commit(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_forward_event(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        flag: ForwardEventFlag,
        xev: C::XEvent,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_set_event_mask(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        forward_event_mask: u32,
        synchronous_event_mask: u32,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_preedit_start(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_preedit_draw(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        caret: i32,
        chg_first: i32,
        chg_len: i32,
        status: PreeditDrawStatus,
        preedit_string: &str,
        feedbacks: Vec<Feedback>,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_preedit_caret(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        position: &mut i32,
        direction: CaretDirection,
        style: CaretStyle,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_preedit_done(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        Ok(())
    }
    fn handle_reset_ic(
        &mut self,
        client: &mut C,
        input_method_id: u16,
        input_context_id: u16,
        preedit_text: &str,
    ) -> Result<(), ClientError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_request, Client, ClientCore, ClientError, ClientHandler};
    use crate::transport::{transport, Transport};
    use crate::AHashMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use xim_parser::{
        Attr, AttributeName, CommitData, Feedback, InputStyle, Point, PreeditDrawStatus, Request,
        XEvent,
    };

    /// A client that sends nothing anywhere: what it was asked to send is
    /// kept.
    #[derive(Default)]
    struct Quiet {
        attributes: AHashMap<AttributeName, u16>,
        sent: Vec<Request>,
    }

    impl ClientCore for Quiet {
        type XEvent = ();
        fn set_attrs(&mut self, _: Vec<Attr>, _: Vec<Attr>) {}
        fn ic_attributes(&self) -> &AHashMap<AttributeName, u16> {
            &self.attributes
        }
        fn im_attributes(&self) -> &AHashMap<AttributeName, u16> {
            &self.attributes
        }
        fn serialize_event(&self, _: &()) -> XEvent {
            unreachable!()
        }
        fn deserialize_event(&self, _: &XEvent) {}
        fn send_req(&mut self, req: Request) -> Result<(), ClientError> {
            self.sent.push(req);
            Ok(())
        }
    }

    /// The text a handler was handed, by what it came as.
    #[derive(Default)]
    struct Texts(Vec<(&'static str, String)>);

    impl ClientHandler<Quiet> for Texts {
        fn handle_commit(&mut self, _: &mut Quiet, _: u16, _: u16, text: &str) -> Result<(), ClientError> {
            self.0.push(("commit", text.into()));
            Ok(())
        }
        fn handle_preedit_draw(
            &mut self,
            _: &mut Quiet,
            _: u16,
            _: u16,
            _: i32,
            _: i32,
            _: i32,
            _: PreeditDrawStatus,
            text: &str,
            _: Vec<Feedback>,
        ) -> Result<(), ClientError> {
            self.0.push(("preedit", text.into()));
            Ok(())
        }
        fn handle_reset_ic(&mut self, _: &mut Quiet, _: u16, _: u16, text: &str) -> Result<(), ClientError> {
            self.0.push(("reset", text.into()));
            Ok(())
        }
    }

    fn draw(bytes: &[u8]) -> Request {
        Request::PreeditDraw {
            input_method_id: 1,
            input_context_id: 1,
            caret: 1,
            chg_first: 0,
            chg_length: 0,
            status: PreeditDrawStatus::empty(),
            preedit_string: bytes.into(),
            feedbacks: Vec::new(),
        }
    }

    /// A preedit, a commit and a reset's preedit that xim-ctext 0.3 could
    /// not decode reach the handler decoded, where the client panicked on
    /// them ("Encoding Error"): a letter of Latin-1 as libX11 writes it,
    /// Korean, and a charset there is no table for.
    #[test]
    fn text_that_did_not_decode_reaches_the_handler() {
        let (mut client, mut handler) = (Quiet::default(), Texts::default());
        let requests = [
            draw(&[0xE9]),
            draw(&[0x1B, 0x24, 0x28, 0x43, 0x47, 0x51, 0x31, 0x39]),
            draw(&[0x1B, 0x24, 0x28, 0x47, 0x44, 0x21]),
            Request::Commit {
                input_method_id: 1,
                input_context_id: 1,
                data: CommitData::Chars { commited: [0x1B, 0x24, 0x28, 0x43, 0x47, 0x51, 0x31, 0x39].into(), syncronous: true },
            },
            Request::ResetIcReply { input_method_id: 1, input_context_id: 1, preedit_string: [0x61, 0xE9].into() },
        ];
        for request in requests {
            handle_request(&mut client, &mut handler, request).unwrap();
        }
        let texts: Vec<(&str, &str)> = handler.0.iter().map(|(k, t)| (*k, t.as_str())).collect();
        assert_eq!(
            texts,
            [("preedit", "é"), ("preedit", "한국"), ("preedit", "\u{FFFD}"), ("commit", "한국"), ("reset", "aé")]
        );
        // a synchronous commit is still answered
        assert!(matches!(client.sent.as_slice(), [Request::SyncReply { .. }]));
    }

    /// What the client sends a server that announced a TRANSPORT_MAX of
    /// 4096, an open, the encoding the open's reply has it ask for, the
    /// input context gpui-pre-linux creates and the spot it moves, and a
    /// sync's reply, goes as the X transports send it: the requests of 20
    /// bytes or fewer in one ClientMessage, the rest by property. Those
    /// between 21 bytes and the limit ended the client on
    /// `todo!("multi-CM")`.
    #[test]
    fn a_request_longer_than_a_client_message_goes_by_property() {
        let mut client = Quiet::default();
        for (name, id) in [
            (AttributeName::InputStyle, 1),
            (AttributeName::ClientWindow, 2),
            (AttributeName::FocusWindow, 3),
            (AttributeName::PreeditAttributes, 4),
            (AttributeName::SpotLocation, 5),
        ] {
            client.attributes.insert(name, id);
        }
        let mut handler = Texts::default();
        client.open("en_US.UTF-8").unwrap();
        let reply = Request::OpenReply { input_method_id: 1, im_attrs: Vec::new(), ic_attrs: Vec::new() };
        handle_request(&mut client, &mut handler, reply).unwrap();
        let spot = |client: &Quiet| {
            client
                .build_ic_attributes()
                .push(AttributeName::InputStyle, InputStyle::PREEDIT_CALLBACKS)
                .push(AttributeName::ClientWindow, 0x0480_0001u32)
                .push(AttributeName::FocusWindow, 0x0480_0001u32)
                .nested_list(AttributeName::PreeditAttributes, |b| {
                    b.push(AttributeName::SpotLocation, Point { x: 120, y: 340 });
                })
                .build()
        };
        let ic = spot(&client);
        client.create_ic(1, ic).unwrap();
        let ic = spot(&client);
        client.set_ic_values(1, 1, ic).unwrap();
        handle_request(&mut client, &mut handler, Request::Sync { input_method_id: 1, input_context_id: 1 }).unwrap();

        let sent: Vec<(&str, Vec<u8>)> = client.sent.iter().map(|r| (r.name(), xim_parser::write_to_vec(r))).collect();
        let names: Vec<&str> = sent.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["Open", "EncodingNegotiation", "CreateIc", "SetIcValues", "SyncReply"]);
        for (name, bytes) in &sent {
            let how = transport(bytes, 4096);
            if bytes.len() <= 20 {
                let mut data = [0u8; 20];
                data[..bytes.len()].copy_from_slice(bytes);
                assert_eq!(how, Transport::ClientMessage(data), "{name}, {} bytes", bytes.len());
            } else {
                assert!(bytes.len() < 4096, "{name}, {} bytes", bytes.len());
                assert_eq!(how, Transport::Property, "{name}, {} bytes", bytes.len());
                // as under ibus's limit of 20
                assert_eq!(transport(bytes, 20), Transport::Property, "{name}");
            }
        }
        // the encoding, the context and its spot are the ones past 20
        let long: Vec<&str> = sent.iter().filter(|(_, b)| b.len() > 20).map(|(n, _)| *n).collect();
        assert_eq!(long, ["EncodingNegotiation", "CreateIc", "SetIcValues"]);
    }
}
