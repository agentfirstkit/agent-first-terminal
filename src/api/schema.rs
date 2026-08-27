use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{MAX_TERMINAL_DIMENSION, MIN_TERMINAL_DIMENSION};

const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Stable `$id` base for the standalone Schemas. It names the contract, not a
/// build machine, and nothing is fetched from it at runtime: every generated
/// Schema is self-contained so the published package reads offline.
const SCHEMA_BASE: &str = "https://agentfirstkit.com/schemas/afterminal/v1";

/// How many nested component expansions one Schema may need. This counts
/// `$ref` hops, not object nesting: the component graph is acyclic, and
/// `standalone_schemas_are_self_contained` fails loudly if a cycle ever makes
/// this cap bite instead of hanging the generator.
const MAX_INLINE_DEPTH: usize = 12;

/// The standalone Schemas, keyed by the filename they are served and exported
/// under.
///
/// One contract source: these are the same component Schemas the OpenAPI
/// document carries, with every internal `$ref` inlined so each file stands on
/// its own.
pub fn standalone_schemas() -> BTreeMap<String, Value> {
    let components = schemas();
    let Some(components) = components.as_object() else {
        return BTreeMap::new();
    };
    components
        .iter()
        .map(|(component, schema)| {
            let filename = format!("{}.schema.json", schema_slug(component));
            let mut schema = inline_component_refs(schema, components, 0);
            if let Some(object) = schema.as_object_mut() {
                object.insert("$schema".to_string(), json!(JSON_SCHEMA_DIALECT));
                object.insert(
                    "$id".to_string(),
                    json!(format!("{SCHEMA_BASE}/{filename}")),
                );
                object.insert("title".to_string(), json!(component));
            }
            (filename, schema)
        })
        .collect()
}

/// The index a caller reads to discover the standalone Schemas.
pub fn schema_index() -> Value {
    let components = schemas();
    let entries = components
        .as_object()
        .map(|components| {
            components
                .keys()
                .map(|component| {
                    let slug = schema_slug(component);
                    json!({
                        "schema_name": slug,
                        "schema_url": format!("/schemas/{slug}.schema.json"),
                        "component_name": component,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "schema_name": "afterminal_schema_index",
        "schema_version": 1,
        "json_schema_dialect": JSON_SCHEMA_DIALECT,
        "count": entries.len(),
        "schemas": entries,
    })
}

/// `SessionInfo` -> `session-info`, so a filename is predictable from the
/// component name without a second hand-kept table.
fn schema_slug(component: &str) -> String {
    let mut slug = String::with_capacity(component.len() + 4);
    for (index, character) in component.char_indices() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
        } else {
            slug.push(character);
        }
    }
    slug
}

fn inline_component_refs(
    value: &Value,
    components: &serde_json::Map<String, Value>,
    depth: usize,
) -> Value {
    if depth > MAX_INLINE_DEPTH {
        return value.clone();
    }
    match value {
        Value::Object(object) => {
            if let Some(component) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
                && let Some(target) = components.get(component)
            {
                return inline_component_refs(target, components, depth + 1);
            }
            Value::Object(
                object
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.clone(),
                            inline_component_refs(value, components, depth),
                        )
                    })
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| inline_component_refs(value, components, depth))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn openapi_document() -> Value {
    json!({
        "openapi": "3.2.0",
        "$self": "/openapi.json",
        "jsonSchemaDialect": JSON_SCHEMA_DIALECT,
        "info": {
            "title": "Agent-First Terminal API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "A bearer-protected local terminal runtime API for opening, observing, and safely coordinating human and agent control of multiple PTY sessions."
        },
        "servers": [{
            "url": "/",
            "description": "The loopback afterminal process that served this document"
        }],
        "tags": [
            {"name": "discovery"},
            {"name": "sessions"},
            {"name": "input"},
            {"name": "secret-input"},
            {"name": "events"}
        ],
        "security": [{"bearerAuth": []}],
        "paths": paths(),
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "Pass the API access token in Authorization. Query-string credentials are not accepted."
                }
            },
            "parameters": {
                "SchemaFile": {
                    "name": "schema_file",
                    "in": "path",
                    "required": true,
                    "description": "Standalone Schema filename from /schemas/index.json",
                    "schema": {
                        "type": "string",
                        "pattern": "^[a-z0-9-]+\\.schema\\.json$"
                    }
                },
                "SessionId": {
                    "name": "session_id",
                    "in": "path",
                    "required": true,
                    "description": "Caller-selected terminal session id",
                    "schema": session_id_schema()
                },
                "LeaseId": {
                    "name": "lease_id",
                    "in": "path",
                    "required": true,
                    "description": "Manager-generated input lease id",
                    "schema": identifier_schema()
                },
                "LastEventId": {
                    "name": "Last-Event-ID",
                    "in": "header",
                    "required": false,
                    "description": "Resume after this global event sequence when it remains in the bounded backlog",
                    "schema": {
                        "type": "string",
                        "pattern": "^[0-9]+$"
                    }
                }
            },
            "schemas": schemas()
        }
    })
}

fn paths() -> Value {
    json!({
        "/health": {
            "get": {
                "operationId": "getHealth",
                "summary": "Check API process health",
                "tags": ["discovery"],
                "security": [],
                "responses": {
                    "200": typed_response("The API process is ready", "HealthResult"),
                    "405": error_response("The HTTP method is not allowed")
                }
            }
        },
        "/openapi.json": {
            "get": {
                "operationId": "getOpenApiDocument",
                "summary": "Read the served OpenAPI contract",
                "tags": ["discovery"],
                "security": [],
                "responses": {
                    "200": {
                        "description": "The generated OpenAPI 3.2 document",
                        "content": {
                            "application/vnd.oai.openapi+json;version=3.2": {
                                "schema": {"type": "object"}
                            }
                        }
                    },
                    "405": error_response("The HTTP method is not allowed")
                }
            }
        },
        "/schemas/index.json": {
            "get": {
                "operationId": "listJsonSchemas",
                "summary": "List the standalone JSON Schemas this process serves",
                "tags": ["discovery"],
                "security": [],
                "responses": {
                    "200": typed_document_response(
                        "The standalone Schema index",
                        "SchemaIndex"
                    ),
                    "405": error_response("The HTTP method is not allowed")
                }
            }
        },
        "/schemas/{schema_file}": {
            "parameters": [{"$ref": "#/components/parameters/SchemaFile"}],
            "get": {
                "operationId": "getJsonSchema",
                "summary": "Read one standalone JSON Schema",
                "tags": ["discovery"],
                "security": [],
                "responses": {
                    "200": {
                        "description": "A self-contained JSON Schema 2020-12 document",
                        "content": {
                            "application/schema+json": {
                                "schema": {"type": "object"}
                            }
                        }
                    },
                    "404": error_response("No Schema is served under that filename"),
                    "405": error_response("The HTTP method is not allowed")
                }
            }
        },
        "/v1/sessions": {
            "get": {
                "operationId": "listSessions",
                "summary": "List every terminal session",
                "tags": ["sessions"],
                "responses": protected_responses("200", "All current sessions", "SessionListResult")
            },
            "post": {
                "operationId": "openSession",
                "summary": "Open a PTY-backed terminal session",
                "tags": ["sessions"],
                "requestBody": request_body("OpenSessionRequest"),
                "responses": protected_responses("200", "The opened session", "SessionInfo")
            }
        },
        "/v1/sessions/{session_id}": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "get": {
                "operationId": "getSession",
                "summary": "Read terminal session metadata",
                "tags": ["sessions"],
                "responses": protected_responses("200", "Current session metadata", "SessionInfo")
            },
            "delete": {
                "operationId": "closeSession",
                "summary": "Kill and remove a terminal session",
                "tags": ["sessions"],
                "responses": no_content_responses("The session was closed")
            }
        },
        "/v1/sessions/{session_id}/screen": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "get": {
                "operationId": "getScreen",
                "summary": "Read the current structured terminal screen",
                "tags": ["sessions"],
                "responses": protected_responses("200", "Current screen snapshot", "ScreenResult")
            }
        },
        "/v1/sessions/{session_id}/input": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "post": {
                "operationId": "sendInput",
                "summary": "Write bytes to a terminal session",
                "description": "Input is base64-encoded and written as one atomic chunk. Non-human actors must provide one of their active shared or exclusive leases; human input may omit a lease and preempts a non-human exclusive holder.",
                "tags": ["input"],
                "requestBody": request_body("SendInputRequest"),
                "responses": protected_responses("200", "The input was accepted", "InputAck")
            }
        },
        "/v1/sessions/{session_id}/resize": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "post": {
                "operationId": "resizeSession",
                "summary": "Resize a terminal session",
                "tags": ["sessions"],
                "requestBody": request_body("ResizeRequest"),
                "responses": protected_responses("200", "Updated session metadata", "SessionInfo")
            }
        },
        "/v1/sessions/{session_id}/signal": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "post": {
                "operationId": "signalSession",
                "summary": "Signal the terminal's foreground process group",
                "description": "Uses the same actor and lease rules as input, then delivers a real operating-system process signal instead of writing a control character. Unix supports interrupt (SIGINT), terminate (SIGTERM), and kill (SIGKILL); other platforms may support only kill.",
                "tags": ["input"],
                "requestBody": request_body("SendSignalRequest"),
                "responses": signal_responses()
            }
        },
        "/v1/sessions/{session_id}/secret-input": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "get": {
                "operationId": "getSecretInput",
                "summary": "Read whether a session is taking secret input",
                "description": "While secret input is on, this session publishes no output, no screen content and no input volume, and every non-human actor is refused input, signals and leases. The reason is operator context; it never carries what is being typed.",
                "tags": ["secret-input"],
                "responses": protected_responses(
                    "200",
                    "Current secret input state",
                    "SecretInputResult"
                )
            }
        },
        "/v1/sessions/{session_id}/secret-input/actions": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "post": {
                "operationId": "secretInputAction",
                "summary": "Start or end secret input mode",
                "description": "Any actor may start a window — raising this shield is the safe direction, so a prompt detector should be able to. Only a human actor may end one: an agent that could end it could simply end it and read the screen.",
                "tags": ["secret-input"],
                "requestBody": request_body("SecretInputActionRequest"),
                "responses": secret_input_responses()
            }
        },
        "/v1/sessions/{session_id}/leases": {
            "parameters": [{"$ref": "#/components/parameters/SessionId"}],
            "get": {
                "operationId": "listInputLeases",
                "summary": "List active input leases",
                "tags": ["input"],
                "responses": protected_responses(
                    "200",
                    "Every unexpired input lease in deterministic order",
                    "InputLeaseListResult"
                )
            },
            "post": {
                "operationId": "acquireInputLease",
                "summary": "Acquire or renew an input lease",
                "description": "Shared leases allow multiple agents to submit atomic chunks. Exclusive leases reserve non-human input for one actor. Repeating the same actor without lease_id renews that actor's existing lease.",
                "tags": ["input"],
                "requestBody": request_body("AcquireInputLeaseRequest"),
                "responses": lease_responses()
            }
        },
        "/v1/sessions/{session_id}/leases/{lease_id}": {
            "parameters": [
                {"$ref": "#/components/parameters/SessionId"},
                {"$ref": "#/components/parameters/LeaseId"}
            ],
            "delete": {
                "operationId": "releaseInputLease",
                "summary": "Release an input lease",
                "tags": ["input"],
                "responses": no_content_responses("The input lease was released")
            }
        },
        "/v1/events": {
            "get": {
                "operationId": "streamEvents",
                "summary": "Watch the multiplexed event stream for every session",
                "description": "Each SSE id is the globally monotonic event sequence. Event payloads identify their session and never contain raw terminal bytes.",
                "tags": ["events"],
                "parameters": [{"$ref": "#/components/parameters/LastEventId"}],
                "responses": {
                    "200": {
                        "description": "A server-sent event stream",
                        "content": {
                            "text/event-stream": {
                                "itemSchema": {"$ref": "#/components/schemas/EventEnvelope"}
                            }
                        }
                    },
                    "401": error_response("A valid bearer credential is required"),
                    "405": error_response("The HTTP method is not allowed"),
                    "500": error_response("The runtime could not serve the event stream")
                }
            }
        }
    })
}

fn request_body(component: &str) -> Value {
    json!({
        "required": true,
        "content": {
            "application/json": {
                "schema": {"$ref": format!("#/components/schemas/{component}")}
            }
        }
    })
}

/// A domain success: the operation's own result Schema, inside the AFDATA
/// result envelope every domain response is serialized in.
fn typed_response(description: &str, component: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "result"},
                        "result": {"$ref": format!("#/components/schemas/{component}")},
                        "trace": {"$ref": "#/components/schemas/Trace"}
                    },
                    "required": ["kind", "result", "trace"],
                    "additionalProperties": false
                }
            }
        }
    })
}

/// A document that is its own contract, served bare under its own media type
/// rather than wrapped in the envelope.
fn typed_document_response(description: &str, component: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": {"$ref": format!("#/components/schemas/{component}")}
            }
        }
    })
}

fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": {"$ref": "#/components/schemas/ApiErrorEnvelope"}
            }
        }
    })
}

fn protected_responses(success_code: &str, description: &str, component: &str) -> Value {
    json!({
        (success_code): typed_response(description, component),
        "400": error_response("The request is invalid"),
        "401": error_response("A valid bearer credential is required"),
        "404": error_response("The session or requested lease does not exist"),
        "409": error_response("The request conflicts with current session or lease state"),
        "413": error_response("The request body or decoded terminal input is too large"),
        "405": error_response("The HTTP method is not allowed"),
        "500": error_response("The terminal runtime failed")
    })
}

fn no_content_responses(description: &str) -> Value {
    json!({
        "204": {"description": description},
        "401": error_response("A valid bearer credential is required"),
        "404": error_response("The session does not exist"),
        "405": error_response("The HTTP method is not allowed"),
        "500": error_response("The terminal runtime failed")
    })
}

fn signal_responses() -> Value {
    json!({
        "200": typed_response(
            "The operating system accepted the signal for delivery",
            "SignalAck"
        ),
        "400": error_response("The request is invalid"),
        "401": error_response("A valid bearer credential is required"),
        "404": error_response("The session or requested lease does not exist"),
        "409": error_response("The process is no longer running or the actor lacks a valid lease"),
        "405": error_response("The HTTP method is not allowed"),
        "500": error_response("The terminal runtime could not deliver the signal"),
        "501": error_response("The signal is not supported on this platform")
    })
}

fn secret_input_responses() -> Value {
    json!({
        "200": typed_response("Secret input state after the action", "SecretInputResult"),
        "400": error_response("The actor or reason is invalid"),
        "401": error_response("A valid bearer credential is required"),
        "403": error_response("Only a human actor may end secret input mode"),
        "404": error_response("The session does not exist"),
        "405": error_response("The HTTP method is not allowed"),
        "409": error_response("The session is still producing output, so the window cannot end yet"),
        "500": error_response("The terminal runtime failed")
    })
}

fn lease_responses() -> Value {
    json!({
        "200": typed_response("The active input lease", "InputLease"),
        "400": error_response("The actor, lease id, or ttl_ms is invalid"),
        "401": error_response("A valid bearer credential is required"),
        "404": error_response("The session or requested lease does not exist"),
        "409": error_response("The requested lease conflicts with another active holder"),
        "405": error_response("The HTTP method is not allowed"),
        "500": error_response("The terminal runtime could not manage the lease")
    })
}

fn session_id_schema() -> Value {
    identifier_schema()
}

fn identifier_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 128,
        "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"
    })
}

fn dimension_schema(default: u16) -> Value {
    json!({
        "type": "integer",
        "minimum": MIN_TERMINAL_DIMENSION,
        "maximum": MAX_TERMINAL_DIMENSION,
        "default": default
    })
}

fn schemas() -> Value {
    let mut components = serde_json::Map::new();
    for group in [
        discovery_schemas(),
        session_schemas(),
        input_schemas(),
        event_schemas(),
    ] {
        if let Value::Object(group) = group {
            components.extend(group);
        }
    }
    Value::Object(components)
}

/// Discovery and envelope shapes.
fn discovery_schemas() -> Value {
    json!({
        "Trace": {
            "type": "object",
            "properties": {
                "duration_ms": {"type": "integer", "minimum": 0}
            },
            "required": ["duration_ms"],
            "additionalProperties": false
        },
        "SchemaIndex": {
            "type": "object",
            "properties": {
                "schema_name": {"const": "afterminal_schema_index"},
                "schema_version": {"type": "integer", "minimum": 1},
                "json_schema_dialect": {"type": "string"},
                "count": {"type": "integer", "minimum": 0},
                "schemas": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "schema_name": {"type": "string", "minLength": 1},
                            "schema_url": {"type": "string", "minLength": 1},
                            "component_name": {"type": "string", "minLength": 1}
                        },
                        "required": ["schema_name", "schema_url", "component_name"],
                        "additionalProperties": false
                    }
                }
            },
            "required": [
                "schema_name",
                "schema_version",
                "json_schema_dialect",
                "count",
                "schemas"
            ],
            "additionalProperties": false
        },
        "HealthResult": {
            "type": "object",
            "properties": {
                "service": {"const": "afterminal"},
                "version": {"type": "string"},
                "status": {"const": "ready"}
            },
            "required": ["service", "version", "status"],
            "additionalProperties": false
        }
    })
}

/// Session lifecycle and secret-input shapes.
fn session_schemas() -> Value {
    json!({
        "OpenSessionRequest": {
            "type": "object",
            "properties": {
                "session_id": session_id_schema(),
                "program": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "description": "Program to execute; null or omitted resolves to the server user's shell"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "default": []
                },
                "cwd_path": {
                    "type": ["string", "null"],
                    "minLength": 1
                },
                "rows": dimension_schema(super::model::DEFAULT_ROWS),
                "cols": dimension_schema(super::model::DEFAULT_COLS),
                "title": {
                    "type": ["string", "null"],
                    "maxLength": 256
                }
            },
            "required": ["session_id"],
            "additionalProperties": false
        },
        "SessionListResult": {
            "type": "object",
            "properties": {
                "sessions": {
                    "type": "array",
                    "items": {"$ref": "#/components/schemas/SessionInfo"}
                }
            },
            "required": ["sessions"],
            "additionalProperties": false
        },
        "SessionInfo": {
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "status": {"type": "string", "enum": ["running", "exited", "error"]},
                "exit_code": {"type": ["integer", "null"]},
                "rows": {"type": "integer", "minimum": 1},
                "cols": {"type": "integer", "minimum": 1},
                "title": {"type": ["string", "null"]},
                "secret_input": {
                    "type": "boolean",
                    "description": "True while a person is entering a secret into this session; its output, screen, and input volume are withheld and non-human actors are suspended"
                }
            },
            "required": [
                "session_id",
                "status",
                "exit_code",
                "rows",
                "cols",
                "title",
                "secret_input"
            ],
            "additionalProperties": false
        },
        "SecretInputResult": {
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "secret_input": {"type": "boolean"},
                "actor": {
                    "oneOf": [
                        {"$ref": "#/components/schemas/InputActor"},
                        {"type": "null"}
                    ],
                    "description": "Who opened the window, absent when it is not open"
                },
                "reason": {
                    "type": ["string", "null"],
                    "maxLength": crate::MAX_SECRET_INPUT_REASON_LEN,
                    "description": "Operator context for the window; never what is being typed"
                }
            },
            "required": ["session_id", "secret_input", "actor", "reason"],
            "additionalProperties": false
        },
        "SecretInputActionRequest": {
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "action": {"const": "start"},
                        "actor": {"$ref": "#/components/schemas/InputActor"},
                        "reason": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": crate::MAX_SECRET_INPUT_REASON_LEN
                        }
                    },
                    "required": ["action", "actor", "reason"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "action": {"const": "end"},
                        "actor": {"$ref": "#/components/schemas/InputActor"}
                    },
                    "required": ["action", "actor"],
                    "additionalProperties": false
                }
            ]
        }
    })
}

/// Input actor, lease, signal, and screen shapes.
fn input_schemas() -> Value {
    json!({
        "InputActorKind": {
            "type": "string",
            "enum": ["human", "agent", "renderer", "controller", "test", "replay"]
        },
        "InputActor": {
            "type": "object",
            "properties": {
                "kind": {"$ref": "#/components/schemas/InputActorKind"},
                "id": identifier_schema()
            },
            "required": ["kind", "id"],
            "additionalProperties": false
        },
        "InputLeaseMode": {
            "type": "string",
            "enum": ["shared", "exclusive"]
        },
        "InputLease": {
            "type": "object",
            "properties": {
                "lease_id": identifier_schema(),
                "actor": {"$ref": "#/components/schemas/InputActor"},
                "mode": {"$ref": "#/components/schemas/InputLeaseMode"},
                "ttl_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": crate::MAX_INPUT_LEASE_TTL_MS
                },
                "remaining_ttl_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": crate::MAX_INPUT_LEASE_TTL_MS
                }
            },
            "required": [
                "lease_id",
                "actor",
                "mode",
                "ttl_ms",
                "remaining_ttl_ms"
            ],
            "additionalProperties": false
        },
        "AcquireInputLeaseRequest": {
            "type": "object",
            "properties": {
                "actor": {"$ref": "#/components/schemas/InputActor"},
                "mode": {"$ref": "#/components/schemas/InputLeaseMode"},
                "ttl_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": crate::MAX_INPUT_LEASE_TTL_MS,
                    "default": crate::DEFAULT_INPUT_LEASE_TTL_MS
                },
                "lease_id": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": 128,
                    "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$",
                    "description": "Existing lease to renew; omit to renew this actor's lease or create one"
                }
            },
            "required": ["actor", "mode"],
            "additionalProperties": false
        },
        "InputLeaseListResult": {
            "type": "object",
            "properties": {
                "leases": {
                    "type": "array",
                    "items": {"$ref": "#/components/schemas/InputLease"}
                }
            },
            "required": ["leases"],
            "additionalProperties": false
        },
        "SendInputRequest": {
            "type": "object",
            "properties": {
                "actor": {"$ref": "#/components/schemas/InputActor"},
                "lease_id": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": 128,
                    "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"
                },
                "data_base64": {
                    "type": "string",
                    "contentEncoding": "base64",
                    "description": "Raw bytes to write to the PTY"
                }
            },
            "required": ["actor", "data_base64"],
            "additionalProperties": false
        },
        "InputAck": {
            "type": "object",
            "properties": {
                "accepted": {"const": true},
                "input_bytes": {"type": "integer", "minimum": 0},
                "actor": {"$ref": "#/components/schemas/InputActor"}
            },
            "required": ["accepted", "input_bytes", "actor"],
            "additionalProperties": false
        },
        "ResizeRequest": {
            "type": "object",
            "properties": {
                "rows": dimension_schema(super::model::DEFAULT_ROWS),
                "cols": dimension_schema(super::model::DEFAULT_COLS)
            },
            "required": ["rows", "cols"],
            "additionalProperties": false
        },
        "TerminalSignal": {
            "type": "string",
            "enum": ["interrupt", "terminate", "kill"],
            "description": "A process signal: interrupt maps to SIGINT, terminate to SIGTERM, and kill to SIGKILL on Unix"
        },
        "SendSignalRequest": {
            "type": "object",
            "properties": {
                "actor": {"$ref": "#/components/schemas/InputActor"},
                "lease_id": {
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": 128,
                    "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"
                },
                "signal": {"$ref": "#/components/schemas/TerminalSignal"}
            },
            "required": ["actor", "signal"],
            "additionalProperties": false
        },
        "SignalAck": {
            "type": "object",
            "properties": {
                "delivered": {"const": true},
                "signal": {"$ref": "#/components/schemas/TerminalSignal"},
                "actor": {"$ref": "#/components/schemas/InputActor"}
            },
            "required": ["delivered", "signal", "actor"],
            "additionalProperties": false
        }
    })
}

/// Screen, event stream, and error shapes.
fn event_schemas() -> Value {
    json!({
        "CursorResult": {
            "type": "object",
            "properties": {
                "row": {"type": "integer", "minimum": 0},
                "col": {"type": "integer", "minimum": 0},
                "visible": {"type": "boolean"}
            },
            "required": ["row", "col", "visible"],
            "additionalProperties": false
        },
        "ActivityResult": {
            "type": "object",
            "properties": {
                "last_output_age_ms": {"type": "integer", "minimum": 0},
                "quiescent": {"type": "boolean"}
            },
            "required": ["last_output_age_ms", "quiescent"],
            "additionalProperties": false
        },
        "ScreenColor": {
            "oneOf": [
                {
                    "type": "object",
                    "properties": {"kind": {"const": "default"}},
                    "required": ["kind"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "indexed"},
                        "index": {"type": "integer", "minimum": 0, "maximum": 255}
                    },
                    "required": ["kind", "index"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "rgb"},
                        "red": {"type": "integer", "minimum": 0, "maximum": 255},
                        "green": {"type": "integer", "minimum": 0, "maximum": 255},
                        "blue": {"type": "integer", "minimum": 0, "maximum": 255}
                    },
                    "required": ["kind", "red", "green", "blue"],
                    "additionalProperties": false
                }
            ]
        },
        "ScreenCell": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "width": {"type": "integer", "enum": [1, 2]},
                "foreground": {"$ref": "#/components/schemas/ScreenColor"},
                "background": {"$ref": "#/components/schemas/ScreenColor"},
                "bold": {"type": "boolean"},
                "dim": {"type": "boolean"},
                "italic": {"type": "boolean"},
                "underline": {"type": "boolean"},
                "inverse": {"type": "boolean"}
            },
            "required": [
                "text", "width", "foreground", "background", "bold", "dim",
                "italic", "underline", "inverse"
            ],
            "additionalProperties": false
        },
        "TerminalModes": {
            "type": "object",
            "properties": {
                "application_cursor": {"type": "boolean"},
                "bracketed_paste": {"type": "boolean"}
            },
            "required": ["application_cursor", "bracketed_paste"],
            "additionalProperties": false
        },
        "ScreenResult": {
            "type": "object",
            "properties": {
                "seq": {"type": "integer", "minimum": 0},
                "cols": {"type": "integer", "minimum": 1},
                "rows": {"type": "integer", "minimum": 1},
                "title": {"type": ["string", "null"]},
                "cursor": {"$ref": "#/components/schemas/CursorResult"},
                "alt_screen": {"type": "boolean"},
                "lines": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Empty while secret_input is true: the screen is withheld, not blank"
                },
                "cells": {
                    "type": "array",
                    "items": {
                        "type": "array",
                        "items": {"$ref": "#/components/schemas/ScreenCell"}
                    },
                    "description": "Styled visible cells; empty while secret_input is true"
                },
                "modes": {"$ref": "#/components/schemas/TerminalModes"},
                "unsupported_extensions": {
                    "type": "array",
                    "items": {"type": "string"}
                },
                "activity": {"$ref": "#/components/schemas/ActivityResult"},
                "secret_input": {
                    "type": "boolean",
                    "description": "True while a person is entering a secret; the rest of this snapshot then describes the session as it was when that window opened"
                }
            },
            "required": [
                "seq",
                "cols",
                "rows",
                "title",
                "cursor",
                "alt_screen",
                "lines",
                "cells",
                "modes",
                "unsupported_extensions",
                "activity",
                "secret_input"
            ],
            "additionalProperties": false
        },
        "EventEnvelope": {
            "type": "object",
            "properties": {
                "seq": {"type": "integer", "minimum": 1},
                "session_id": {"type": "string"},
                "event": {"$ref": "#/components/schemas/TerminalEvent"}
            },
            "required": ["seq", "session_id", "event"],
            "additionalProperties": false
        },
        "TerminalEvent": {
            "oneOf": [
                {
                    "type": "object",
                    "properties": {"type": {"const": "session_opened"}},
                    "required": ["type"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "screen_changed"},
                        "screen_seq": {"type": "integer", "minimum": 1}
                    },
                    "required": ["type", "screen_seq"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "output_chunk"},
                        "chunk_bytes": {"type": "integer", "minimum": 1}
                    },
                    "required": ["type", "chunk_bytes"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "resized"},
                        "rows": {"type": "integer", "minimum": 1},
                        "cols": {"type": "integer", "minimum": 1}
                    },
                    "required": ["type", "rows", "cols"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "input_accepted"},
                        "actor": {"$ref": "#/components/schemas/InputActor"},
                        "input_bytes": {"type": "integer", "minimum": 0},
                        "lease_id": {
                            "type": ["string", "null"],
                            "minLength": 1
                        }
                    },
                    "required": ["type", "actor", "input_bytes", "lease_id"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "input_rejected"},
                        "actor": {"$ref": "#/components/schemas/InputActor"},
                        "reason": {
                            "type": "string",
                            "enum": [
                                "lease_required",
                                "lease_not_found",
                                "lease_conflict",
                                "secret_input_active"
                            ]
                        }
                    },
                    "required": ["type", "actor", "reason"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "input_preempted"},
                        "previous_actor": {"$ref": "#/components/schemas/InputActor"},
                        "by_actor": {"$ref": "#/components/schemas/InputActor"},
                        "lease_id": {"type": "string", "minLength": 1}
                    },
                    "required": [
                        "type",
                        "previous_actor",
                        "by_actor",
                        "lease_id"
                    ],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "input_lease_acquired"},
                        "lease": {"$ref": "#/components/schemas/InputLease"}
                    },
                    "required": ["type", "lease"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "input_lease_released"},
                        "lease_id": {"type": "string", "minLength": 1},
                        "actor": {"$ref": "#/components/schemas/InputActor"},
                        "reason": {
                            "type": "string",
                            "enum": ["released", "expired", "human_preempted"]
                        }
                    },
                    "required": ["type", "lease_id", "actor", "reason"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "signal_sent"},
                        "signal": {"$ref": "#/components/schemas/TerminalSignal"},
                        "actor": {
                            "oneOf": [
                                {"$ref": "#/components/schemas/InputActor"},
                                {"type": "null"}
                            ]
                        },
                        "lease_id": {
                            "type": ["string", "null"],
                            "minLength": 1
                        }
                    },
                    "required": ["type", "signal", "actor", "lease_id"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "secret_input_started"},
                        "actor": {"$ref": "#/components/schemas/InputActor"},
                        "reason": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": crate::MAX_SECRET_INPUT_REASON_LEN
                        }
                    },
                    "required": ["type", "actor", "reason"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "secret_input_ended"},
                        "actor": {"$ref": "#/components/schemas/InputActor"}
                    },
                    "required": ["type", "actor"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "process_exited"},
                        "exit_code": {"type": ["integer", "null"]}
                    },
                    "required": ["type", "exit_code"],
                    "additionalProperties": false
                }
            ]
        },
        "ApiErrorEnvelope": {
            "type": "object",
            "properties": {
                "kind": {"const": "error"},
                "error": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string", "minLength": 1},
                        "message": {"type": "string", "minLength": 1},
                        "retryable": {"type": "boolean"},
                        "hint": {"type": "string", "minLength": 1}
                    },
                    "required": ["code", "message", "retryable"],
                    "additionalProperties": true
                },
                "trace": {"$ref": "#/components/schemas/Trace"}
            },
            "required": ["kind", "error", "trace"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use serde_json::{Value, json};

    use super::{JSON_SCHEMA_DIALECT, openapi_document, schema_index, standalone_schemas};

    #[test]
    fn operation_ids_are_unique_and_component_refs_resolve() {
        let document = openapi_document();
        assert_eq!(document["openapi"], "3.2.0");
        let component_names = document["components"]["schemas"]
            .as_object()
            .map(|schemas| schemas.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let mut operation_ids = BTreeSet::new();
        visit(&document["paths"], &mut |value| {
            if let Some(operation_id) = value.get("operationId").and_then(Value::as_str) {
                assert!(operation_ids.insert(operation_id.to_string()));
            }
            if let Some(reference) = value.get("$ref").and_then(Value::as_str)
                && let Some(component) = reference.strip_prefix("#/components/schemas/")
            {
                assert!(
                    component_names.contains(component),
                    "missing schema {component}"
                );
            }
        });
        assert_eq!(operation_ids.len(), 18);
    }

    #[test]
    fn standalone_schemas_are_self_contained_and_indexed() {
        let schemas = standalone_schemas();
        let component_names = openapi_document()["components"]["schemas"]
            .as_object()
            .map(|schemas| schemas.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        assert_eq!(schemas.len(), component_names.len());
        for (filename, schema) in &schemas {
            assert!(filename.ends_with(".schema.json"), "{filename}");
            assert_eq!(schema["$schema"], JSON_SCHEMA_DIALECT, "{filename}");
            assert_eq!(
                schema["$id"],
                format!("https://agentfirstkit.com/schemas/afterminal/v1/{filename}"),
                "{filename}"
            );
            // A published contract has to read offline, so nothing may point
            // at another file — or at a `$ref` the inliner failed to resolve.
            let mut references = Vec::new();
            visit(schema, &mut |value| {
                if let Some(reference) = value.get("$ref") {
                    references.push(reference.clone());
                }
            });
            assert!(references.is_empty(), "{filename} still has {references:?}");
        }

        let index = schema_index();
        assert_eq!(index["count"], schemas.len());
        assert_eq!(index["json_schema_dialect"], JSON_SCHEMA_DIALECT);
        let indexed = index["schemas"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry["schema_url"].as_str())
                    .map(|url| url.trim_start_matches("/schemas/").to_string())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            indexed,
            schemas.keys().cloned().collect::<BTreeSet<_>>(),
            "the index and the served Schemas disagree"
        );
    }

    #[test]
    fn every_domain_success_is_an_afdata_result_envelope() {
        let document = openapi_document();
        let mut checked = 0;
        let Some(paths) = document["paths"].as_object() else {
            panic!("the document has no paths");
        };
        for (path, item) in paths {
            // Discovery documents are their own contract and are served bare.
            if !path.starts_with("/v1/") && path != "/health" {
                continue;
            }
            let Some(item) = item.as_object() else {
                continue;
            };
            for (method, operation) in item {
                if method == "parameters" {
                    continue;
                }
                let success = &operation["responses"]["200"];
                if success.is_null() {
                    continue;
                }
                // A stream is described by its itemSchema, not by a finite
                // result envelope.
                if !success["content"]["text/event-stream"]["itemSchema"].is_null() {
                    continue;
                }
                let schema = &success["content"]["application/json"]["schema"];
                assert_eq!(schema["properties"]["kind"]["const"], "result", "{path}");
                assert!(
                    schema["properties"]["result"]["$ref"].is_string(),
                    "{path} does not reference its own result schema"
                );
                checked += 1;
            }
        }
        assert!(checked >= 10, "only {checked} operations were checked");
        assert_eq!(
            document["components"]["schemas"]["ApiErrorEnvelope"]["properties"]["kind"]["const"],
            "error"
        );
    }

    #[test]
    fn sse_items_and_bearer_auth_are_part_of_the_contract() {
        let document = openapi_document();
        assert_eq!(
            document["paths"]["/v1/events"]["get"]["responses"]["200"]["content"]["text/event-stream"]
                ["itemSchema"]["$ref"],
            "#/components/schemas/EventEnvelope"
        );
        assert_eq!(
            document["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        assert_eq!(
            document["paths"]["/openapi.json"]["get"]["security"],
            json!([])
        );
        assert_eq!(
            document["paths"]["/v1/sessions/{session_id}/signal"]["post"]["operationId"],
            "signalSession"
        );
        assert_eq!(
            document["components"]["schemas"]["TerminalSignal"]["enum"],
            json!(["interrupt", "terminate", "kill"])
        );
        assert_eq!(
            document["paths"]["/v1/sessions/{session_id}/leases"]["post"]["operationId"],
            "acquireInputLease"
        );
        assert_eq!(
            document["components"]["schemas"]["SendInputRequest"]["required"],
            json!(["actor", "data_base64"])
        );
        assert_eq!(
            document["components"]["schemas"]["InputLeaseMode"]["enum"],
            json!(["shared", "exclusive"])
        );
    }

    #[test]
    fn committed_contract_matches_rust_source() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi");
        assert_eq!(
            read_json(&directory.join("openapi.json")),
            openapi_document()
        );
        assert_eq!(
            read_json(&directory.join("schemas/index.json")),
            schema_index()
        );
        let schemas = standalone_schemas();
        for (filename, schema) in &schemas {
            assert_eq!(
                &read_json(&directory.join("schemas").join(filename)),
                schema,
                "{filename}"
            );
        }
        let committed = std::fs::read_dir(directory.join("schemas"))
            .expect("read committed schema directory")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|filename| filename.ends_with(".schema.json"))
            .collect::<BTreeSet<_>>();
        assert_eq!(committed, schemas.keys().cloned().collect::<BTreeSet<_>>());
    }

    fn read_json(path: &Path) -> Value {
        let bytes =
            std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    fn visit(value: &Value, callback: &mut impl FnMut(&Value)) {
        callback(value);
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, callback);
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, callback);
                }
            }
            _ => {}
        }
    }
}
