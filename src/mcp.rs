use crate::api;
use crate::server::RequestExt;
use hyper::body::HttpBody;
use hyper::{Body, Method, Request, Response, StatusCode};
use route_recognizer::Params;
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_MESSAGE_BYTES: usize = 1_048_576;

#[derive(Clone, Copy)]
enum ApiHandler {
	Search,
	Posts,
	Comments,
	People,
	Subreddits,
	Post,
	SubredditAbout,
	UserAbout,
	UserProfile,
}

fn response(status: StatusCode, body: Value) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "application/json; charset=utf-8")
		.body(Body::from(body.to_string()))
		.unwrap_or_default()
}

fn empty(status: StatusCode) -> Response<Body> {
	Response::builder().status(status).body(Body::empty()).unwrap_or_default()
}

pub async fn method_not_allowed(_: Request<Body>) -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(StatusCode::METHOD_NOT_ALLOWED)
			.header("allow", "POST")
			.body(Body::empty())
			.unwrap_or_default(),
	)
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_result(value: Value, is_error: bool) -> Value {
	let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
	json!({
		"content": [{ "type": "text", "text": text }],
		"isError": is_error,
	})
}

fn tool_error(message: impl Into<String>) -> Value {
	tool_result(json!({ "error": message.into() }), true)
}

fn string_arg(args: &Value, name: &str) -> Result<Option<String>, String> {
	match args.get(name) {
		None | Some(Value::Null) => Ok(None),
		Some(Value::String(value)) => Ok(Some(value.clone())),
		Some(Value::Number(value)) => Ok(Some(value.to_string())),
		Some(Value::Bool(value)) => Ok(Some(value.to_string())),
		Some(_) => Err(format!("Argument `{name}` must be a string.")),
	}
}

fn required_string(args: &Value, name: &str) -> Result<String, String> {
	string_arg(args, name)?
		.filter(|value| !value.trim().is_empty())
		.ok_or_else(|| format!("Argument `{name}` is required."))
}

fn safe_segment(value: &str, label: &str) -> Result<(), String> {
	if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
		return Err(format!("`{label}` may contain only letters, numbers, `_`, and `-`."));
	}
	Ok(())
}

fn query_uri(path: &str, pairs: &[(&str, String)]) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	for (key, value) in pairs {
		serializer.append_pair(key, value);
	}
	let query = serializer.finish();
	if query.is_empty() {
		path.to_string()
	} else {
		format!("{path}?{query}")
	}
}

fn make_api_request(uri: String, params: &[(&str, &str)]) -> Request<Body> {
	let mut request = Request::builder().method(Method::GET).uri(uri).body(Body::empty()).unwrap_or_default();
	let mut route_params = Params::new();
	for (key, value) in params {
		route_params.insert((*key).to_string(), (*value).to_string());
	}
	request.set_params(route_params);
	request
}

async fn call_api(handler: ApiHandler, request: Request<Body>) -> (StatusCode, Value) {
	let result = match handler {
		ApiHandler::Search => api::search(request).await,
		ApiHandler::Posts => api::posts(request).await,
		ApiHandler::Comments => api::search_comments(request).await,
		ApiHandler::People => api::search_people(request).await,
		ApiHandler::Subreddits => api::search_subreddits(request).await,
		ApiHandler::Post => api::post(request).await,
		ApiHandler::SubredditAbout => api::about(request).await,
		ApiHandler::UserAbout => api::user_about(request).await,
		ApiHandler::UserProfile => api::profile(request).await,
	};
	match result {
		Ok(response) => {
			let status = response.status();
			let body = hyper::body::to_bytes(response.into_body()).await.unwrap_or_default();
			(
				status,
				serde_json::from_slice(&body).unwrap_or_else(|_| json!({ "error": "Redlib returned a non-JSON response." })),
			)
		}
		Err(error) => (StatusCode::BAD_GATEWAY, json!({ "error": error })),
	}
}

async fn call_tool(name: &str, args: &Value) -> Value {
	let result: Result<(ApiHandler, String, Vec<(String, String)>, Vec<(String, String)>), String> = (|| {
		let mut route = Vec::new();
		let mut query = Vec::new();
		let (handler, path) = match name {
			"reddit_search_posts" => {
				let q = required_string(args, "query")?;
				query.push(("q".to_string(), q));
				for (argument, parameter) in [("sort", "sort"), ("limit", "limit"), ("after", "after")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				if let Some(value) = args.get("restrict_sr") {
					let value = match value {
						Value::Bool(true) => "on".to_string(),
						Value::Bool(false) => "off".to_string(),
						_ => string_arg(args, "restrict_sr")?.unwrap_or_default(),
					};
					query.push(("restrict_sr".to_string(), value));
				}
				if let Some(subreddit) = string_arg(args, "subreddit")? {
					safe_segment(&subreddit, "subreddit")?;
					route.push(("sub".to_string(), subreddit.clone()));
					(ApiHandler::Search, format!("/api/r/{subreddit}/search"))
				} else {
					(ApiHandler::Search, "/api/search".to_string())
				}
			}
			"reddit_get_subreddit_posts" => {
				let subreddit = required_string(args, "subreddit")?;
				safe_segment(&subreddit, "subreddit")?;
				let sort = string_arg(args, "sort")?.unwrap_or_else(|| "hot".to_string());
				if !["hot", "new", "top", "rising", "controversial"].contains(&sort.as_str()) {
					return Err("`sort` must be hot, new, top, rising, or controversial.".to_string());
				}
				route.extend([("sub".to_string(), subreddit.clone()), ("sort".to_string(), sort.clone())]);
				for (argument, parameter) in [("limit", "limit"), ("after", "after"), ("flair", "flair")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				(ApiHandler::Posts, format!("/api/r/{subreddit}/{sort}"))
			}
			"reddit_search_recent_comments" => {
				let subreddit = required_string(args, "subreddit")?;
				safe_segment(&subreddit, "subreddit")?;
				route.push(("sub".to_string(), subreddit.clone()));
				query.push(("q".to_string(), required_string(args, "query")?));
				for (argument, parameter) in [("limit", "limit"), ("after", "after")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				(ApiHandler::Comments, format!("/api/r/{subreddit}/comments/search"))
			}
			"reddit_search_users" | "reddit_search_subreddits" => {
				query.push(("q".to_string(), required_string(args, "query")?));
				for (argument, parameter) in [("limit", "limit"), ("after", "after")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				if name == "reddit_search_users" {
					(ApiHandler::People, "/api/people/search".to_string())
				} else {
					(ApiHandler::Subreddits, "/api/subreddits/search".to_string())
				}
			}
			"reddit_get_post_thread" => {
				let id = required_string(args, "post_id")?;
				safe_segment(&id, "post_id")?;
				for (argument, parameter) in [("sort", "sort"), ("expand", "expand"), ("budget", "budget")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				let subreddit = string_arg(args, "subreddit")?;
				let path = if let Some(subreddit) = subreddit {
					safe_segment(&subreddit, "subreddit")?;
					route.extend([("sub".to_string(), subreddit.clone()), ("id".to_string(), id.clone())]);
					format!("/api/r/{subreddit}/comments/{id}")
				} else {
					route.push(("id".to_string(), id.clone()));
					format!("/api/comments/{id}")
				};
				(ApiHandler::Post, path)
			}
			"reddit_get_subreddit_about" => {
				let subreddit = required_string(args, "subreddit")?;
				safe_segment(&subreddit, "subreddit")?;
				route.push(("sub".to_string(), subreddit.clone()));
				(ApiHandler::SubredditAbout, format!("/api/r/{subreddit}/about"))
			}
			"reddit_get_user_about" => {
				let username = required_string(args, "username")?;
				safe_segment(&username, "username")?;
				route.push(("name".to_string(), username.clone()));
				(ApiHandler::UserAbout, format!("/api/user/{username}/about"))
			}
			"reddit_get_user_listing" => {
				let username = required_string(args, "username")?;
				safe_segment(&username, "username")?;
				let listing = string_arg(args, "listing")?.unwrap_or_else(|| "posts".to_string());
				if !["posts", "comments"].contains(&listing.as_str()) {
					return Err("`listing` must be posts or comments.".to_string());
				}
				route.extend([("name".to_string(), username.clone()), ("listing".to_string(), listing.clone())]);
				for (argument, parameter) in [("limit", "limit"), ("after", "after")] {
					if let Some(value) = string_arg(args, argument)? {
						query.push((parameter.to_string(), value));
					}
				}
				(ApiHandler::UserProfile, format!("/api/user/{username}/{listing}"))
			}
			_ => return Err(format!("Unknown tool `{name}`.")),
		};
		Ok((handler, path, route, query))
	})();

	let (handler, path, route, query) = match result {
		Ok(result) => result,
		Err(message) => return tool_error(message),
	};
	let route_refs: Vec<(&str, &str)> = route.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
	let query_refs: Vec<(&str, String)> = query.iter().map(|(key, value)| (key.as_str(), value.clone())).collect();
	let request = make_api_request(query_uri(&path, &query_refs), &route_refs);
	let (status, value) = call_api(handler, request).await;
	tool_result(value, !status.is_success())
}

fn tool_definitions() -> Value {
	json!([
		{
			"name": "reddit_search_posts",
			"description": "Search Reddit posts, optionally scoped to a subreddit. Results and ranking are controlled by Reddit.",
			"inputSchema": { "type": "object", "properties": {
				"query": { "type": "string", "description": "Search terms." },
				"subreddit": { "type": "string", "description": "Optional subreddit scope." },
				"sort": { "type": "string", "description": "Reddit search sort." },
				"limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum results requested." },
				"after": { "type": "string", "description": "Reddit pagination cursor from a previous response." },
				"restrict_sr": { "type": "boolean", "description": "Restrict results to the subreddit." }
			}, "required": ["query"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_get_subreddit_posts",
			"description": "List posts from a subreddit using hot, new, top, rising, or controversial sorting.",
			"inputSchema": { "type": "object", "properties": {
				"subreddit": { "type": "string" }, "sort": { "type": "string", "enum": ["hot", "new", "top", "rising", "controversial"] },
			"limit": { "type": "integer", "minimum": 1, "maximum": 100 }, "after": { "type": "string" }, "flair": { "type": "string" }
			}, "required": ["subreddit"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_search_recent_comments",
			"description": "Search comment text within one page of a subreddit’s recent comments. This is not a global or historical comment search.",
			"inputSchema": { "type": "object", "properties": {
				"subreddit": { "type": "string" }, "query": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Number of recent comments to scan (max 100)." }, "after": { "type": "string" }
			}, "required": ["subreddit", "query"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_search_users",
			"description": "Search Reddit’s user index. Results are ranked by Reddit and are not an exhaustive directory.",
			"inputSchema": { "type": "object", "properties": { "query": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 100 }, "after": { "type": "string" } }, "required": ["query"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_search_subreddits",
			"description": "Search Reddit’s subreddit index. Results are ranked by Reddit and are not an exhaustive directory.",
			"inputSchema": { "type": "object", "properties": { "query": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 100 }, "after": { "type": "string" } }, "required": ["query"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_get_post_thread",
			"description": "Fetch a Reddit post and its comment tree. Hidden replies can be expanded with a bounded request budget.",
			"inputSchema": { "type": "object", "properties": {
				"post_id": { "type": "string" }, "subreddit": { "type": "string", "description": "Optional subreddit, useful for quarantined communities." },
				"sort": { "type": "string" }, "expand": { "type": "boolean", "description": "Fetch hidden replies." }, "budget": { "type": "integer", "minimum": 0, "maximum": 50, "description": "Maximum morechildren requests (hard-capped at 50)." }
			}, "required": ["post_id"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_get_subreddit_about",
			"description": "Fetch public metadata for a subreddit.",
			"inputSchema": { "type": "object", "properties": { "subreddit": { "type": "string" } }, "required": ["subreddit"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_get_user_about",
			"description": "Fetch a public Reddit user profile summary.",
			"inputSchema": { "type": "object", "properties": { "username": { "type": "string" } }, "required": ["username"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		},
		{
			"name": "reddit_get_user_listing",
			"description": "Fetch a user's recent posts or comments.",
			"inputSchema": { "type": "object", "properties": {
				"username": { "type": "string" }, "listing": { "type": "string", "enum": ["posts", "comments"] },
				"limit": { "type": "integer", "minimum": 1, "maximum": 100 }, "after": { "type": "string" }
			}, "required": ["username"] },
			"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true }
		}
	])
}

async fn process_message(message: Value) -> Option<Value> {
	let id = message.get("id").cloned();
	let Some(object) = message.as_object() else {
		return Some(rpc_error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
	};
	if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
		return Some(rpc_error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
	}
	let Some(method) = object.get("method").and_then(Value::as_str) else {
		return Some(rpc_error(id.unwrap_or(Value::Null), -32600, "Invalid Request"));
	};
	let Some(id) = id else {
		return None;
	};
	let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
	let result = match method {
		"initialize" => {
			let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
			if !["2024-11-05", "2025-03-26", "2025-06-18", PROTOCOL_VERSION].contains(&requested) {
				return Some(rpc_error(id, -32602, "Unsupported MCP protocol version."));
			}
			json!({
				"protocolVersion": requested,
				"capabilities": { "tools": { "listChanged": false } },
				"serverInfo": { "name": "redlib-mcp", "version": env!("CARGO_PKG_VERSION") },
				"instructions": "Read-only Reddit tools backed by Redlib. Reddit controls upstream availability, search ranking, and pagination."
			})
		}
		"ping" => json!({}),
		"tools/list" => json!({ "tools": tool_definitions() }),
		"tools/call" => {
			let name = params.get("name").and_then(Value::as_str);
			let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
			match name {
				Some(name) if args.is_object() => call_tool(name, &args).await,
				Some(_) => tool_error("Tool arguments must be a JSON object."),
				None => tool_error("Tool name is required."),
			}
		}
		"notifications/initialized" | "notifications/cancelled" => return None,
		_ if method.starts_with("notifications/") => return None,
		_ => return Some(rpc_error(id, -32601, "Method not found")),
	};
	Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub async fn handle(request: Request<Body>) -> Result<Response<Body>, String> {
	if let Some(response) = api::unauthorized_response(&request) {
		return Ok(response);
	}
	if !request
		.headers()
		.get("content-type")
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"))
	{
		return Ok(response(StatusCode::UNSUPPORTED_MEDIA_TYPE, json!({ "error": "MCP requests must use application/json." })));
	}
	let mut body = request.into_body();
	let mut bytes = Vec::new();
	while let Some(chunk) = body.data().await {
		let chunk = match chunk {
			Ok(chunk) => chunk,
			Err(_) => return Ok(response(StatusCode::BAD_REQUEST, json!({ "error": "Unable to read MCP request body." }))),
		};
		if bytes.len() + chunk.len() > MAX_MESSAGE_BYTES {
			return Ok(response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": "MCP request body exceeds 1 MiB." })));
		}
		bytes.extend_from_slice(&chunk);
	}
	let message: Value = match serde_json::from_slice(&bytes) {
		Ok(message) => message,
		Err(_) => return Ok(response(StatusCode::OK, rpc_error(Value::Null, -32700, "Parse error"))),
	};
	let replies = match message {
		Value::Array(messages) if messages.is_empty() => vec![rpc_error(Value::Null, -32600, "Invalid Request")],
		Value::Array(messages) => {
			let mut replies = Vec::with_capacity(messages.len());
			for message in messages {
				if let Some(reply) = process_message(message).await {
					replies.push(reply);
				}
			}
			replies
		}
		message => process_message(message).await.into_iter().collect(),
	};
	if replies.is_empty() {
		return Ok(empty(StatusCode::ACCEPTED));
	}
	let body = if replies.len() == 1 {
		replies.into_iter().next().unwrap_or(Value::Null)
	} else {
		Value::Array(replies)
	};
	Ok(response(StatusCode::OK, body))
}
