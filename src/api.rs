use crate::client::json;
use crate::server::RequestExt;
use crate::subreddit::can_access_quarantine;
use hyper::{Body, Request, Response, StatusCode};
use log::warn;
use serde_json::{json, Value};

const POST_SORTS: [&str; 5] = ["hot", "new", "top", "rising", "controversial"];

// PROJECTORS: select a stable subset of Reddit's raw JSON fields, keeping native types.

fn post_json(post: &Value) -> Value {
	let d = &post["data"];
	json!({
		"kind": post["kind"],
		"id": d["id"],
		"name": d["name"],
		"title": d["title"],
		"author": d["author"],
		"author_flair_text": d["author_flair_text"],
		"subreddit": d["subreddit"],
		"permalink": d["permalink"],
		"url": d["url"],
		"domain": d["domain"],
		"score": d["score"],
		"upvote_ratio": d["upvote_ratio"],
		"num_comments": d["num_comments"],
		"created_utc": d["created_utc"],
		"edited": d["edited"],
		"selftext": d["selftext"],
		"is_self": d["is_self"],
		"stickied": d["stickied"],
		"nsfw": d["over_18"],
		"spoiler": d["spoiler"],
		"link_flair_text": d["link_flair_text"],
		"post_hint": d["post_hint"],
		"thumbnail": d["thumbnail"],
		"media": d["media"],
		"secure_media": d["secure_media"],
	})
}

fn comment_json(comment: &Value) -> Value {
	let d = &comment["data"];
	let replies = match &d["replies"] {
		Value::Object(obj) => map_children(&obj["data"]["children"]),
		_ => Vec::new(),
	};
	json!({
		"kind": comment["kind"],
		"id": d["id"],
		"name": d["name"],
		"parent_id": d["parent_id"],
		"link_id": d["link_id"],
		"permalink": d["permalink"],
		"author": d["author"],
		"author_fullname": d["author_fullname"],
		"subreddit": d["subreddit"],
		"body": d["body"],
		"created_utc": d["created_utc"],
		"edited": d["edited"],
		"score": d["score"],
		"stickied": d["stickied"],
		"is_submitter": d["is_submitter"],
		"distinguished": d["distinguished"],
		"replies": replies,
	})
}

fn person_json(person: &Value) -> Value {
	let d = &person["data"];
	let community = &d["subreddit"];
	let created_utc = d["created_utc"].as_f64().or_else(|| d["created"].as_f64()).map_or(Value::Null, Value::from);
	let total_karma = d["total_karma"].as_i64().unwrap_or_else(|| d["link_karma"].as_i64().unwrap_or_default() + d["comment_karma"].as_i64().unwrap_or_default());
	json!({
		"kind": "user",
		"id": d["id"],
		"name": d["name"],
		"username": d["name"],
		"profile_url": format!("/user/{}", d["name"].as_str().unwrap_or_default()),
		"created_utc": created_utc,
		"link_karma": d["link_karma"],
		"comment_karma": d["comment_karma"],
		"total_karma": total_karma,
		"icon": d["icon_img"],
		"title": community["title"],
		"public_description": community["public_description"],
		"nsfw": community["over_18"],
	})
}

// "more" nodes mark comment threads Reddit did not inline; `count` says how many are hidden.
fn more_json(node: &Value) -> Value {
	let d = &node["data"];
	json!({
		"kind": "more",
		"id": d["id"],
		"parent_id": d["parent_id"],
		"count": d["count"],
		"children": d["children"],
	})
}

fn map_children(children: &Value) -> Vec<Value> {
	children.as_array().map_or_else(Vec::new, |arr| {
		arr.iter()
			.filter_map(|c| match c["kind"].as_str() {
				Some("t3") => Some(post_json(c)),
				Some("t1") => Some(comment_json(c)),
				Some("more") => Some(more_json(c)),
				_ => None,
			})
			.collect()
	})
}

// RESPONSE HELPERS

fn json_response(status: StatusCode, body: Value) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("content-type", "application/json; charset=utf-8")
		.body(body.to_string().into())
		.unwrap_or_default()
}

fn api_error(msg: &str) -> Response<Body> {
	let (status, hint) = match msg {
		"quarantined" => (StatusCode::FORBIDDEN, "Quarantined subreddit: accept it once via POST /r/{sub}, then retry."),
		"gated" => (StatusCode::FORBIDDEN, "Gated subreddit access is not supported through the API."),
		"private" => (StatusCode::NOT_FOUND, "Private subreddit."),
		"banned" => (StatusCode::NOT_FOUND, "Banned subreddit."),
		"suspended" => (StatusCode::NOT_FOUND, "Suspended user."),
		_ => (StatusCode::BAD_GATEWAY, "Upstream Reddit error."),
	};
	json_response(status, json!({ "error": msg, "detail": hint }))
}

async fn listing(path: String, quarantined: bool, flair: Option<&str>) -> Response<Body> {
	match json(path, quarantined).await {
		Ok(v) => {
			let data = &v["data"];
			let mut children = map_children(&data["children"]);
			if let Some(f) = flair {
				children.retain(|c| c["link_flair_text"].as_str().map_or(false, |t| t.to_lowercase().contains(&f.to_lowercase())));
			}
			json_response(StatusCode::OK, json!({
				"after": data["after"],
				"before": data["before"],
				"count": children.len(),
				"children": children,
			}))
		}
		Err(e) => api_error(&e),
	}
}

fn query_param(req: &Request<Body>, name: &str) -> Option<String> {
	url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
		.find(|(k, _)| k == name)
		.map(|(_, v)| v.into_owned())
}

fn bounded_limit(req: &Request<Body>, default: usize, max: usize) -> usize {
	query_param(req, "limit")
		.and_then(|v| v.parse::<usize>().ok())
		.unwrap_or(default)
		.clamp(1, max)
}

fn encode_query_value(value: &str) -> String {
	url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn invalid(param: &str, allowed: &[&str]) -> Response<Body> {
	json_response(StatusCode::BAD_REQUEST, json!({ "error": format!("Invalid {param}"), "allowed": allowed }))
}

// HANDLERS

// GET /api/r/:sub[/:sort]?limit=&after=&t=&flair=
pub async fn posts(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let sort = req.param("sort").unwrap_or_else(|| "hot".to_string());
	if !POST_SORTS.contains(&sort.as_str()) {
		return Ok(invalid("sort", &POST_SORTS));
	}
	let flair = query_param(&req, "flair");
	let path = format!("/r/{}/{}.json?{}&raw_json=1", sub.replace('+', "%2B"), sort, req.uri().query().unwrap_or_default());
	Ok(listing(path, can_access_quarantine(&req, &sub), flair.as_deref()).await)
}

// Comment-tree expansion via Reddit's morechildren endpoint. Reddit omits deeply nested or
// long-thread comments, marking them with "more" nodes; this inlines them within a request budget.
struct ExpandBudget {
	requests: u32,
}

fn expand_nodes<'a>(nodes: &'a mut Vec<Value>, link_id: &'a str, sort: &'a str, count: &'a str, budget: &'a mut ExpandBudget) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
	Box::pin(async move {
	// Breadth-first: every `more` node at this level claims chunks from the shared budget
	// in list order, all chunks are fetched concurrently, then subtrees expand recursively.
	// A sequential walk would pay a full Reddit round-trip per chunk.
	let mut planned: Vec<(usize, String)> = Vec::new();
	for (idx, node) in nodes.iter().enumerate() {
		if node["kind"].as_str() != Some("more") {
			continue;
		}
		let ids: Vec<&str> = node["children"].as_array().map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
		for chunk in ids.chunks(100) {
			if budget.requests == 0 {
				break;
			}
			budget.requests -= 1;
			// morechildren expects bare basenames; link_id wants the full t3_ name
			let path = format!("/api/morechildren?api_type=json&link_id={link_id}&sort={sort}&count={count}&children={}", chunk.join(","));
			planned.push((idx, path));
		}
	}

	let owners: Vec<usize> = planned.iter().map(|(idx, _)| *idx).collect();
	let mut results: Vec<Vec<Value>> = (0..planned.len()).map(|_| Vec::new()).collect();
	let mut set = tokio::task::JoinSet::new();
	for (seq, (_idx, path)) in planned.into_iter().enumerate() {
		set.spawn(async move {
			let batch = match json(path, false).await {
				Ok(v) => map_children(&v["json"]["data"]["things"]),
				Err(e) => {
					warn!("morechildren fetch failed: {e}");
					Vec::new()
				}
			};
			(seq, batch)
		});
	}
	while let Some(res) = set.join_next().await {
		if let Ok((seq, batch)) = res {
			results[seq] = batch;
		}
	}

	// Regroup by originating node, preserving chunk order.
	let mut grouped: std::collections::HashMap<usize, Vec<Vec<Value>>> = std::collections::HashMap::new();
	for (seq, batch) in results.into_iter().enumerate() {
		grouped.entry(owners[seq]).or_default().push(batch);
	}

	let current = std::mem::take(nodes);
	for (idx, mut node) in current.into_iter().enumerate() {
		let kind = node["kind"].as_str().unwrap_or_default().to_string();
		match kind.as_str() {
			"more" => {
				match grouped.remove(&idx) {
					Some(batches) => {
						let mut fetched: Vec<Value> = batches.concat();
						expand_nodes(&mut fetched, link_id, sort, count, budget).await;
						nodes.extend(fetched);
					}
					None => nodes.push(node),
				}
			}
			"t1" => {
				if node["replies"].is_array() {
					let mut replies = std::mem::replace(&mut node["replies"], Value::Array(Vec::new()));
					if let Value::Array(ref mut r) = replies {
						expand_nodes(r, link_id, sort, count, budget).await;
					}
					node["replies"] = replies;
				}
				nodes.push(node);
			}
			_ => nodes.push(node),
		}
	}
})
}

// GET /api/r/:sub/comments/:id  or  GET /api/comments/:id  (?expand=1 inlines hidden reply threads)
pub async fn post(req: Request<Body>) -> Result<Response<Body>, String> {
	let id = req.param("id").unwrap_or_default();
	let sub = req.param("sub").unwrap_or_default();
	let expand = query_param(&req, "expand").map_or(false, |v| v == "1" || v == "true");
	let path = format!("/comments/{id}.json?{}&raw_json=1", req.uri().query().unwrap_or_default());
	Ok(match json(path, can_access_quarantine(&req, &sub)).await {
		Ok(v) => {
			let post = &v[0]["data"]["children"][0];
			let mut comments = map_children(&v[1]["data"]["children"]);
			let mut remaining_more = None;
			if expand {
				let link_id = post["data"]["name"].as_str().unwrap_or_default().to_string();
				let sort = query_param(&req, "sort").unwrap_or_else(|| "confidence".to_string());
				let count = post["data"]["num_comments"].as_i64().unwrap_or(0).to_string();
				let mut budget = ExpandBudget { requests: query_param(&req, "budget").and_then(|v| v.parse().ok()).unwrap_or(10).min(50) };
				expand_nodes(&mut comments, &link_id, &sort, &count, &mut budget).await;
				remaining_more = Some(count_more_nodes(&comments));
			}
			json_response(StatusCode::OK, json!({
				"post": post_json(post),
				"comments": comments,
				"meta": { "expanded": expand, "unexpanded_more_nodes": remaining_more },
			}))
		}
		Err(e) => api_error(&e),
	})
}

fn count_more_nodes(nodes: &[Value]) -> usize {
	nodes
		.iter()
		.map(|n| match n["kind"].as_str() {
			Some("more") => 1,
			Some("t1") => count_more_nodes(n["replies"].as_array().map_or(&[], |a| a.as_slice())),
			_ => 0,
		})
		.sum()
}

// GET /api/r/:sub/about
pub async fn about(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let path = format!("/r/{}/about.json?raw_json=1", sub.replace('+', "%2B"));
	Ok(match json(path, can_access_quarantine(&req, &sub)).await {
		Ok(v) => {
			let d = &v["data"];
			json_response(StatusCode::OK, json!({
				"id": d["id"],
				"name": d["display_name"],
				"title": d["title"],
				"public_description": d["public_description"],
				"description": d["description"],
				"subscribers": d["subscriber_count"],
				"active_users": d["active_user_count"],
				"created_utc": d["created_utc"],
				"nsfw": d["over_18"],
				"icon": d["icon_img"],
				"community_icon": d["community_icon"],
			}))
		}
		Err(e) => api_error(&e),
	})
}

// GET /api/search?q=[&restrict_sr=on&subreddit=…]
pub async fn search(req: Request<Body>) -> Result<Response<Body>, String> {
	let query = req.uri().query().unwrap_or_default();
	if !query.contains("q=") {
		return Ok(json_response(StatusCode::BAD_REQUEST, json!({ "error": "Missing required query parameter q" })));
	}
	let (path, sub) = match req.param("sub") {
		Some(sub) => (format!("/r/{}/search.json?{}&raw_json=1", sub.replace('+', "%2B"), query), sub),
		None => (format!("/search.json?{query}&raw_json=1"), String::new()),
	};
	Ok(listing(path, can_access_quarantine(&req, &sub), None).await)
}

// GET /api/r/:sub/comments/search?q=...&limit=...&after=...
// Reddit has no dependable global comment-text search endpoint. This scans one
// bounded page from the subreddit comments feed and filters its bodies locally.
pub async fn search_comments(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let query = query_param(&req, "q").unwrap_or_default();
	if query.trim().is_empty() {
		return Ok(json_response(StatusCode::BAD_REQUEST, json!({ "error": "Missing required query parameter q" })));
	}
	let scan_limit = bounded_limit(&req, 100, 100);
	let after = query_param(&req, "after").unwrap_or_default();
	let mut params = vec![format!("limit={scan_limit}"), "raw_json=1".to_string()];
	if !after.is_empty() {
		params.push(format!("after={}", encode_query_value(&after)));
	}
	let path = format!("/r/{}/comments.json?{}", sub.replace('+', "%2B"), params.join("&"));
	Ok(match json(path, can_access_quarantine(&req, &sub)).await {
		Ok(v) => {
			let data = &v["data"];
			let query_lower = query.to_lowercase();
			let scanned = data["children"].as_array().map_or(0, Vec::len);
			let children: Vec<Value> = data["children"]
				.as_array()
				.into_iter()
				.flatten()
				.filter(|item| {
					item["kind"].as_str() == Some("t1")
						&& item["data"]["body"].as_str().is_some_and(|body| body.to_lowercase().contains(&query_lower))
				})
				.map(comment_json)
				.collect();
			json_response(StatusCode::OK, json!({
				"query": query,
				"scope": format!("r/{sub}"),
				"mode": "recent_comments_filtered",
				"scanned": scanned,
				"count": children.len(),
				"after": data["after"],
				"before": data["before"],
				"children": children,
			}))
		}
		Err(e) => api_error(&e),
	})
}

// GET /api/people/search?q=...&limit=...&after=...
// Uses Reddit's own type=user search results; matching and completeness are
// therefore bounded by Reddit's search index and ranking.
pub async fn search_people(req: Request<Body>) -> Result<Response<Body>, String> {
	let query = query_param(&req, "q").unwrap_or_default();
	if query.trim().is_empty() {
		return Ok(json_response(StatusCode::BAD_REQUEST, json!({ "error": "Missing required query parameter q" })));
	}
	let limit = bounded_limit(&req, 25, 100);
	let after = query_param(&req, "after").unwrap_or_default();
	let mut params = vec![format!("q={}", encode_query_value(&query)), "type=user".to_string(), format!("limit={limit}"), "raw_json=1".to_string()];
	if !after.is_empty() {
		params.push(format!("after={}", encode_query_value(&after)));
	}
	let path = format!("/search.json?{}", params.join("&"));
	Ok(match json(path, false).await {
		Ok(v) => {
			let data = &v["data"];
			let children: Vec<Value> = data["children"]
				.as_array()
				.into_iter()
				.flatten()
				.filter(|item| item["kind"].as_str() == Some("t2"))
				.map(person_json)
				.collect();
			json_response(StatusCode::OK, json!({
				"query": query,
				"mode": "reddit_user_search",
				"count": children.len(),
				"after": data["after"],
				"before": data["before"],
				"children": children,
			}))
		}
		Err(e) => api_error(&e),
	})
}

// GET /api/user/:name/about
pub async fn user_about(req: Request<Body>) -> Result<Response<Body>, String> {
	let name = req.param("name").unwrap_or_default();
	let path = format!("/user/{}/about.json?raw_json=1", encode_query_value(&name));
	Ok(match json(path, false).await {
		Ok(person) => json_response(StatusCode::OK, person_json(&person)),
		Err(e) => api_error(&e),
	})
}

// GET /api/user/:name[/:listing]?limit=&after=   (listing: posts | comments)
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let name = req.param("name").unwrap_or_default();
	let listing_kind = req.param("listing").unwrap_or_else(|| "posts".to_string());
	const LISTINGS: [&str; 2] = ["posts", "comments"];
	if !LISTINGS.contains(&listing_kind.as_str()) {
		return Ok(invalid("listing", &LISTINGS));
	}
	let path = format!("/user/{name}/{listing_kind}.json?{}&raw_json=1", req.uri().query().unwrap_or_default());
	Ok(listing(path, false, None).await)
}

// GET /api — endpoint index
pub async fn index(_: Request<Body>) -> Result<Response<Body>, String> {
	Ok(json_response(StatusCode::OK, json!({
		"endpoints": {
			"/api/r/{subreddit}/{sort}": "Post listing. sort: hot|new|top|rising|controversial. Params: limit, after, t (for top/controversial), flair (filter by flair text).",
			"/api/r/{subreddit}/comments/{id}": "One post plus its comment tree. Params: sort, limit, depth, context, expand=1 (inline hidden reply threads).",
			"/api/comments/{id}": "Same, subreddit inferred.",
			"/api/r/{subreddit}/about": "Subreddit metadata.",
			"/api/search": "Search Reddit. Params: q (required), restrict_sr, subreddit, sort, limit, after.",
			"/api/r/{subreddit}/comments/search": "Search comment text in one bounded page of recent subreddit comments. Params: q (required), limit (max 100 scanned), after. Not a global or historical comment search.",
			"/api/people/search": "Search Reddit users via Reddit's type=user search. Params: q (required), limit (max 100), after. Results follow Reddit's index and ranking and are not exhaustive.",
			"/api/user/{username}/about": "Public user profile summary.",
			"/api/user/{username}/{listing}": "User listing. listing: posts|comments. Params: limit, after.",
		},
		"pagination": "Pass the previous response's 'after' as ?after=",
	})))
}
