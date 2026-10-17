"""Exercise the real MCP SDK over an in-memory HTTP transport."""

import asyncio
import json
from pathlib import Path
import tempfile
import traceback
import unittest
from unittest.mock import patch

import httpx

import parallel_sources as sources


class SourceCollectionTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.output = Path(self.temp.name) / "sources.md"
        self.requests = []
        self.started = asyncio.Event()
        self.cancelled = asyncio.Event()
        self.mode = "success"

    async def respond(self, request):
        self.requests.append(request)
        self.assertEqual(str(request.url), sources.ENDPOINT)
        self.assertEqual(request.headers["user-agent"], sources.USER_AGENT)
        self.assertNotIn("authorization", request.headers)
        self.assertNotIn("cookie", request.headers)
        message = json.loads(await request.aread())
        if message["method"] == "notifications/initialized":
            return httpx.Response(202)
        if message["method"] == "initialize":
            result = {
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"},
            }
        elif message["method"] == "tools/list":
            result = {"tools": [
                {"name": name, "inputSchema": {"type": "object"}}
                for name in ("web_search", "web_fetch")
            ]}
        else:
            self.assertEqual(message["method"], "tools/call")
            name = message["params"]["name"]
            if self.mode == "wait":
                self.started.set()
                try:
                    await asyncio.Event().wait()
                finally:
                    self.cancelled.set()
            payload = {
                "web_search": {"search_id": "search_fixture", "results": [{
                    "url": "https://docs.rs/tokio/", "title": "Tokio docs",
                    "excerpts": ["Cancellation safety guidance"],
                }]},
                "web_fetch": {"extract_id": "extract_fixture", "results": [{
                    "url": "https://docs.rs/tokio/", "title": "Tokio docs",
                    "excerpts": ["A cancelled future is dropped."],
                }], "errors": []},
            }[name]
            if self.mode == "page_error" and name == "web_fetch":
                payload["results"] = []
                payload["errors"] = [{"url": "https://docs.rs/tokio/", "error_type": "FETCH_ERROR"}]
            if self.mode == "partial_error" and name == "web_fetch":
                payload["errors"] = [{"url": "https://other.example/", "error_type": "FETCH_ERROR"}]
            if self.mode == "no_results":
                payload["results"] = []
            text = json.dumps(payload)
            result = {"content": [{"type": "text", "text": text}]}
            if self.mode == "tool_error" and name == "web_fetch":
                result = {"isError": True, "content": [{"type": "text", "text": "Fetch failed"}]}
            if self.mode == "empty":
                result = {"content": []}
        return httpx.Response(200, json={"jsonrpc": "2.0", "id": message["id"], "result": result})

    async def run_example(self, *extra):
        real_client = httpx.AsyncClient

        def client_factory(**kwargs):
            return real_client(transport=httpx.MockTransport(self.respond), **kwargs)

        with patch.object(sources.httpx, "AsyncClient", client_factory):
            await sources.main([
                "Find cancellation safety guidance", "--query", "tokio cancellation safety",
                "--output", str(self.output), *extra,
            ])

    def tool_calls(self):
        messages = [json.loads(request.content) for request in self.requests]
        return [message["params"] for message in messages if message["method"] == "tools/call"]

    async def test_search_and_fetch_write_useful_source_file(self):
        await self.run_example("--query", "tokio select drop", "--url", "https://docs.rs/tokio/")
        text = self.output.read_text()
        self.assertIn("Cancellation safety guidance", text)
        self.assertIn("A cancelled future is dropped.", text)
        self.assertIn("https://docs.rs/tokio/", text)
        search, fetch = self.tool_calls()
        self.assertEqual(search["arguments"]["objective"], "Find cancellation safety guidance")
        self.assertEqual(search["arguments"]["search_queries"],
                         ["tokio cancellation safety", "tokio select drop"])
        self.assertEqual(fetch["arguments"]["urls"], ["https://docs.rs/tokio/"])
        self.assertEqual(search["arguments"]["session_id"], fetch["arguments"]["session_id"])

    async def test_search_only_does_not_fetch(self):
        await self.run_example()
        self.assertEqual([call["name"] for call in self.tool_calls()], ["web_search"])
        self.assertIn("Cancellation safety guidance", self.output.read_text())

    async def test_fetch_error_preserves_existing_file(self):
        self.mode = "tool_error"
        self.output.write_text("previous sources")
        with self.assertRaises(Exception) as caught:
            await self.run_example("--url", "https://docs.rs/tokio/")
        self.assertIn("web_fetch failed: Fetch failed", "".join(traceback.format_exception(caught.exception)))
        self.assertEqual(self.output.read_text(), "previous sources")

    async def test_empty_response_does_not_create_file(self):
        self.mode = "empty"
        with self.assertRaises(Exception) as caught:
            await self.run_example()
        self.assertIn("web_search failed: no text returned", "".join(traceback.format_exception(caught.exception)))
        self.assertFalse(self.output.exists())

    async def test_embedded_page_errors_preserve_existing_file(self):
        for mode in ("page_error", "partial_error"):
            with self.subTest(mode=mode):
                self.mode = mode
                self.output.write_text("previous sources")
                with self.assertRaises(Exception) as caught:
                    await self.run_example("--url", "https://docs.rs/tokio/")
                self.assertIn("FETCH_ERROR", "".join(traceback.format_exception(caught.exception)))
                self.assertEqual(self.output.read_text(), "previous sources")

    async def test_no_search_results_preserves_existing_file(self):
        self.mode = "no_results"
        self.output.write_text("previous sources")
        with self.assertRaises(Exception) as caught:
            await self.run_example()
        self.assertIn("web_search failed: errors or no results", "".join(traceback.format_exception(caught.exception)))
        self.assertEqual(self.output.read_text(), "previous sources")

    async def test_deadline_cancels_request_and_preserves_file(self):
        self.mode = "wait"
        self.output.write_text("previous sources")
        with self.assertRaises(TimeoutError):
            await self.run_example("--timeout", "0.2")
        self.assertTrue(self.started.is_set())
        self.assertTrue(self.cancelled.is_set())
        self.assertEqual(self.output.read_text(), "previous sources")

    async def test_cancellation_stops_request_without_writing(self):
        self.mode = "wait"
        task = asyncio.create_task(self.run_example())
        await asyncio.wait_for(self.started.wait(), timeout=2)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(self.cancelled.is_set())
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
