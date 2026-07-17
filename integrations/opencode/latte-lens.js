// Latte Lens observability bridge for OpenCode's documented local plugin API.
// Copy this file to .opencode/plugins/latte-lens.js or the global plugin directory.

const OBSERVER = "opencode/plugin"
const TIMEOUT_MILLIS = 1_000

export const LatteLensPlugin = async ({ directory }) => {
  const binary = process.env.LATTE_LENS_BIN || "latte-lens"
  const currentTurns = new Map()
  const bridgeInstance = globalThis.crypto?.randomUUID?.() || `${Date.now()}-${Math.random()}`
  let sequence = 0

  const eventID = (event) => {
    if (event && typeof event.id === "string" && event.id.length > 0) return event.id
    sequence += 1
    return `${bridgeInstance}:${sequence}`
  }

  const emit = async (hookEventName, sessionID, fields = {}, nativeEvent) => {
    if (typeof sessionID !== "string" || sessionID.length === 0) return
    const payload = JSON.stringify({
      session_id: sessionID,
      hook_event_name: hookEventName,
      event_id: eventID(nativeEvent),
      ...fields,
    })

    let child
    let timeout
    try {
      child = Bun.spawn(
        [
          binary,
          "hook",
          "--observer",
          OBSERVER,
          "--event",
          hookEventName,
          "--workspace",
          directory,
        ],
        { stdin: "pipe", stdout: "ignore", stderr: "ignore" },
      )
      child.stdin.write(payload)
      child.stdin.end()
      timeout = setTimeout(() => child.kill(), TIMEOUT_MILLIS)
      await child.exited
    } catch {
      // Observability is fail-open and must never change OpenCode behavior.
    } finally {
      if (timeout) clearTimeout(timeout)
    }
  }

  return {
    event: async ({ event }) => {
      const properties = event?.properties
      switch (event?.type) {
        case "session.created":
        case "session.updated":
        case "session.deleted": {
          const info = properties?.info
          if (!info || typeof info.id !== "string") return
          if (event.type === "session.deleted") currentTurns.delete(info.id)
          await emit(
            event.type,
            info.id,
            typeof info.parentID === "string" ? { parent_session_id: info.parentID } : {},
            event,
          )
          return
        }
        case "message.updated": {
          const info = properties?.info
          if (!info || info.role !== "user") return
          if (typeof info.sessionID !== "string" || typeof info.id !== "string") return
          currentTurns.set(info.sessionID, info.id)
          await emit(event.type, info.sessionID, { turn_id: info.id }, event)
          return
        }
        case "session.status": {
          const sessionID = properties?.sessionID
          const status = properties?.status?.type
          if (!["busy", "retry", "idle"].includes(status)) return
          const turnID = currentTurns.get(sessionID)
          if (status === "idle") currentTurns.delete(sessionID)
          await emit(
            event.type,
            sessionID,
            { status, ...(turnID ? { turn_id: turnID } : {}) },
            event,
          )
          return
        }
        case "session.error": {
          const sessionID = properties?.sessionID
          const turnID = currentTurns.get(sessionID)
          if (!turnID) return
          currentTurns.delete(sessionID)
          await emit(event.type, sessionID, { turn_id: turnID }, event)
          return
        }
        case "permission.asked": {
          const sessionID = properties?.sessionID
          const permissionID = properties?.id
          if (typeof permissionID !== "string") return
          const turnID = currentTurns.get(sessionID)
          await emit(
            event.type,
            sessionID,
            { permission_id: permissionID, ...(turnID ? { turn_id: turnID } : {}) },
            event,
          )
          return
        }
        case "permission.replied": {
          const sessionID = properties?.sessionID
          const permissionID = properties?.requestID
          const reply = properties?.reply
          if (typeof permissionID !== "string" || !["once", "always", "reject"].includes(reply)) return
          const turnID = currentTurns.get(sessionID)
          await emit(
            event.type,
            sessionID,
            { permission_id: permissionID, reply, ...(turnID ? { turn_id: turnID } : {}) },
            event,
          )
          return
        }
        case "message.part.updated": {
          const part = properties?.part
          if (!part || part.type !== "tool" || part.state?.status !== "error") return
          const turnID = currentTurns.get(part.sessionID)
          await emit(
            event.type,
            part.sessionID,
            {
              tool_call_id: part.callID,
              tool_name: part.tool,
              status: "error",
              ...(turnID ? { turn_id: turnID } : {}),
            },
            event,
          )
          return
        }
      }
    },

    "tool.execute.before": async (input) => {
      const turnID = currentTurns.get(input.sessionID)
      await emit("tool.execute.before", input.sessionID, {
        tool_call_id: input.callID,
        tool_name: input.tool,
        ...(turnID ? { turn_id: turnID } : {}),
      })
    },

    "tool.execute.after": async (input) => {
      const turnID = currentTurns.get(input.sessionID)
      await emit("tool.execute.after", input.sessionID, {
        tool_call_id: input.callID,
        tool_name: input.tool,
        ...(turnID ? { turn_id: turnID } : {}),
      })
    },
  }
}
