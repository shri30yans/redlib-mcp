# Redlib MCP

Redlib MCP is a Rust server that exposes Reddit reading and search through the [Model Context Protocol](https://modelcontextprotocol.io/) and a small JSON API. It uses Redlib’s existing Reddit client, caching, OAuth handling, and rate-limit tracking.

The server speaks stateless Streamable HTTP at `POST /mcp`. It does not serve Redlib’s website. MCP requests call the same Rust API handlers directly, with no second Python service and no loopback HTTP calls.

## MCP tools

| Tool | What it does |
| --- | --- |
| `reddit_search_posts` | Search posts, optionally scoped to a subreddit. |
| `reddit_get_subreddit_posts` | Read a subreddit listing with pagination and optional flair filtering. |
| `reddit_search_recent_comments` | Search comment text in one bounded page of recent subreddit comments. |
| `reddit_search_users` | Search Reddit’s user index. |
| `reddit_search_subreddits` | Search Reddit’s subreddit index. |
| `reddit_get_post_thread` | Read a post and its comments; optionally expand hidden replies with a request budget. |
| `reddit_get_subreddit_about` | Read public subreddit metadata. |
| `reddit_get_user_about` | Read a public user summary. |
| `reddit_get_user_listing` | Read a user’s recent posts or comments. |

Search results, pagination, and availability follow Reddit’s upstream behavior. User and subreddit search use Reddit’s indexes; comment-text search scans only a bounded recent feed and is not global or historical.

## Run it

Build and start the server with Rust 1.85 or newer:

```sh
cargo build --release
./target/release/redlib-mcp --address 127.0.0.1 --port 8080
```

The MCP endpoint is `http://127.0.0.1:8080/mcp`. For an MCP client that accepts Streamable HTTP servers, configure that URL. For example:

```json
{
  "mcpServers": {
    "reddit": {
      "url": "http://127.0.0.1:8080/mcp"
    }
  }
}
```

Client configuration formats vary. If the server is reachable beyond your own machine, set `REDLIB_API_TOKEN`; the same bearer token protects `/mcp` and `/api`:

```sh
REDLIB_API_TOKEN='replace-with-a-long-random-secret' ./target/release/redlib-mcp --address 0.0.0.0
```

Clients must send `Authorization: Bearer <token>`. Without `REDLIB_API_TOKEN`, both endpoints are open to requests that can reach the listener.

### Docker

```sh
docker build -t redlib-mcp .
docker run --rm -p 8080:8080 -e REDLIB_API_TOKEN='replace-with-a-long-random-secret' redlib-mcp
```

## JSON API

The REST API remains available alongside MCP:

| Endpoint | Purpose |
| --- | --- |
| `GET /api` | List API endpoints. |
| `GET /api/search?q=...` | Search Reddit posts. |
| `GET /api/r/{subreddit}/{sort}` | Read subreddit posts. |
| `GET /api/r/{subreddit}/comments/search?q=...` | Search one page of recent subreddit comments. |
| `GET /api/people/search?q=...` | Search Reddit users. |
| `GET /api/subreddits/search?q=...` | Search Reddit communities. |
| `GET /api/r/{subreddit}/comments/{id}` | Read a post and its comments. |
| `GET /api/user/{username}/about` | Read a public user summary. |
| `GET /api/user/{username}/{posts\|comments}` | Read a user listing. |

API responses are JSON and use Reddit’s pagination cursor in `after`.

## Project notes

This project is based on [Redlib](https://github.com/redlib-org/redlib) and retains its AGPL-3.0-only license. See [CREDITS](CREDITS) for upstream credits. It is an independent MCP and JSON API server; it is not affiliated with Reddit.
