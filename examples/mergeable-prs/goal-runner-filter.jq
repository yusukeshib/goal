def compact_image_payload:
  if type == "object" and .type == "image" and (.data? | type) == "string" then
    {
      type: .type,
      mimeType: (.mimeType // .mime_type // null),
      encoded_size_bytes: (.data | length)
    }
  else
    .
  end;

def compact_tool_images:
  if .type == "tool_execution_end" and (.result?.content? | type) == "array" then
    .result.content |= map(compact_image_payload)
  else
    .
  end;

select(
  .type != "message_update"
  and .type != "tool_execution_update"
  and .type != "message_start"
  and (
    .type != "message_end"
    or (
      .message?.role != "user"
      and .message?.role != "toolResult"
    )
  )
)
| compact_tool_images
| if .type == "turn_end" then
    del(.message, .toolResults)
  elif .type == "agent_end" then
    del(.messages)
  else
    .
  end
