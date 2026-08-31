import json
import shutil
import subprocess
import unittest
from pathlib import Path


FILTER_PATH = Path(__file__).with_name("goal-runner-filter.jq")


@unittest.skipUnless(shutil.which("jq"), "jq is required")
class GoalRunnerFilterTests(unittest.TestCase):
    def test_removes_duplicate_messages_and_compacts_images(self):
        events = [
            {"type": "message_update", "delta": "large"},
            {"type": "tool_execution_update", "partialResult": "large"},
            {"type": "message_start", "message": {"role": "assistant"}},
            {"type": "message_end", "message": {"role": "user", "content": "prompt"}},
            {
                "type": "message_end",
                "message": {"role": "toolResult", "content": "duplicate"},
            },
            {
                "type": "message_end",
                "message": {"role": "assistant", "content": "kept"},
            },
            {
                "type": "tool_execution_end",
                "result": {
                    "content": [
                        {"type": "text", "text": "kept"},
                        {
                            "type": "image",
                            "mimeType": "image/png",
                            "data": "abcdef",
                        },
                    ]
                },
            },
            {"type": "turn_end", "message": "large", "toolResults": ["large"]},
            {"type": "agent_end", "messages": ["large"]},
        ]
        process = subprocess.run(
            ["jq", "--compact-output", "--from-file", str(FILTER_PATH)],
            input="\n".join(json.dumps(event) for event in events),
            text=True,
            capture_output=True,
            check=True,
        )
        filtered = [json.loads(line) for line in process.stdout.splitlines()]

        self.assertEqual(
            [event["type"] for event in filtered],
            ["message_end", "tool_execution_end", "turn_end", "agent_end"],
        )
        image = filtered[1]["result"]["content"][1]
        self.assertEqual(
            image,
            {
                "type": "image",
                "mimeType": "image/png",
                "encoded_size_bytes": 6,
            },
        )
        self.assertNotIn("message", filtered[2])
        self.assertNotIn("toolResults", filtered[2])
        self.assertNotIn("messages", filtered[3])


if __name__ == "__main__":
    unittest.main()
