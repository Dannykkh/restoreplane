//! User-process commands never enter the privileged ApplicationCommand dispatcher.
use restoreplane_domain::workbench::WBError;
use restoreplane_domain::IpcRequestId as RequestId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! commands {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub enum WorkbenchCommand { $(#[serde(rename = $wire)] $variant),+ }
        impl WorkbenchCommand {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            pub fn parse(value: &str) -> Result<Self, WBError> { match value { $($wire => Ok(Self::$variant)),+, _ => Err(WBError::new("WB_PAYLOAD_INVALID")) } }
        }
    }
}

commands! {
    CredentialList => "credential.list", CredentialPut => "credential.put", CredentialDelete => "credential.delete",
    SessionOpen => "session.open", SessionList => "session.list", SessionReconnect => "session.reconnect", SessionClose => "session.close", SessionModeSet => "session.mode.set",
    CatalogPage => "catalog.page", CatalogDescribe => "catalog.describe", CatalogGraph => "catalog.graph",
    BrowseOpen => "browse.open", BrowsePage => "browse.page", SnapshotBegin => "snapshot.begin", SnapshotEnd => "snapshot.end",
    QueryReview => "query.review", QueryRun => "query.run", QueryResults => "query.results", QueryCancel => "query.cancel",
    OperationPrepare => "operation.prepare", OperationGet => "operation.get", OperationList => "operation.list", OperationStatements => "operation.statements", OperationAcknowledgeUnknown => "operation.acknowledge_unknown",
    TransactionBegin => "transaction.begin", TransactionGet => "transaction.get", TransactionCommit => "transaction.commit", TransactionRollback => "transaction.rollback",
    ResultAttach => "result.attach", ResultFrames => "result.frames", ResultAck => "result.ack", ResultPage => "result.page", ResultValueGet => "result.value.get",
    EditPrepare => "edit.prepare", EditApply => "edit.apply", EditDiscard => "edit.discard",
    EditConflictGet => "edit.conflict.get",
    MovementSourcePick => "movement.source.pick", MovementDestinationPick => "movement.destination.pick", MovementFileRelease => "movement.file.release", MovementStatus => "movement.status", MovementPartialRemove => "movement.partial.remove",
    ImportPreview => "import.preview", ImportStart => "import.start", ImportContinue => "import.continue", ImportCancel => "import.cancel",
    ImportContinueReview => "import.continue.review",
    ExportStart => "export.start", ExportCancel => "export.cancel",
    SavedQueryList => "saved_query.list", SavedQueryGet => "saved_query.get", SavedQueryPut => "saved_query.put", SavedQueryDelete => "saved_query.delete",
    ProtectionSelectionGet => "protection.selection.get",
}

impl WorkbenchCommand {
    pub const fn requires_attempt(self) -> bool {
        matches!(
            self,
            Self::QueryRun
                | Self::TransactionCommit
                | Self::EditApply
                | Self::ImportStart
                | Self::ImportContinue
        )
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub command: WorkbenchCommand,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_token: Option<String>,
}

impl WorkbenchRequest {
    /// Parse before Tauri/serde_json::Value can discard duplicate object keys.
    pub fn decode_json(raw: &str) -> Result<Self, WBError> {
        if raw.len() > 1024 * 1024 {
            return Err(WBError::new("WB_LIMIT"));
        }
        use serde::de::DeserializeSeed;
        let mut parser = serde_json::Deserializer::from_str(raw);
        let value = StrictValue(0)
            .deserialize(&mut parser)
            .map_err(|_| WBError::new("WB_PAYLOAD_INVALID"))?;
        parser
            .end()
            .map_err(|_| WBError::new("WB_PAYLOAD_INVALID"))?;
        let request: Self =
            serde_json::from_value(value).map_err(|_| WBError::new("WB_PAYLOAD_INVALID"))?;
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<(), WBError> {
        if self.protocol_version != 1 {
            return Err(WBError::new("WB_PROTOCOL_UNSUPPORTED"));
        }
        if !self.payload.is_object()
            || self
                .attempt_token
                .as_ref()
                .is_some_and(|t| t.is_empty() || t.len() > 512)
        {
            return Err(WBError::new("WB_PAYLOAD_INVALID"));
        }
        if self.command.requires_attempt() && self.attempt_token.is_none() {
            return Err(WBError::new("WB_PAYLOAD_INVALID").with_reason("native_attempt_required"));
        }
        if serde_json::to_vec(&self.payload)
            .map_err(|_| WBError::new("WB_PAYLOAD_INVALID"))?
            .len()
            > 1024 * 1024
        {
            return Err(WBError::new("WB_LIMIT"));
        }
        Ok(())
    }
}

struct StrictValue(usize);
impl<'de> serde::de::DeserializeSeed<'de> for StrictValue {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.0 > 32 {
            return Err(serde::de::Error::custom("depth limit"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for StrictValue {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded JSON without duplicate keys")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictValue(self.0 + 1))? {
            if values.len() >= 32768 {
                return Err(serde::de::Error::custom("array limit"));
            }
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.len() >= 32768 || values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate key or object limit"));
            }
            values.insert(key, map.next_value_seed(StrictValue(self.0 + 1))?);
        }
        Ok(Value::Object(values))
    }
}

impl std::fmt::Debug for WorkbenchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkbenchRequest")
            .field("request_id", &self.request_id)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_commands_cannot_be_misrouted_to_service() {
        for command in WorkbenchCommand::ALL {
            assert!(crate::ApplicationCommand::try_from(command.as_str()).is_err());
            assert_eq!(WorkbenchCommand::parse(command.as_str()).unwrap(), *command);
        }
        assert!(WorkbenchCommand::parse("native.exec").is_err());
    }
    #[test]
    fn request_rejects_forged_authority_and_redacts_body() {
        let request = serde_json::json!({"protocolVersion":1,"requestId":restoreplane_domain::IpcRequestId::new(),"command":"credential.put","payload":{"password":"do-not-log-secret"}});
        let decoded: WorkbenchRequest = serde_json::from_value(request.clone()).unwrap();
        assert!(!format!("{decoded:?}").contains("do-not-log-secret"));
        let mut forged = request;
        forged["ownerSid"] = "fake".into();
        assert!(serde_json::from_value::<WorkbenchRequest>(forged).is_err());
    }
    #[test]
    fn raw_decoder_rejects_duplicates_before_value_conversion() {
        let id = restoreplane_domain::IpcRequestId::new();
        for body in [
            r#"{"scope":{"workspaceId":"first","workspaceId":"second"}}"#,
            r#"{"value":1,"value":2}"#,
        ] {
            let raw = format!(
                r#"{{"protocolVersion":1,"requestId":"{id}","command":"session.list","payload":{body}}}"#
            );
            assert!(WorkbenchRequest::decode_json(&raw).is_err());
        }
    }
}
