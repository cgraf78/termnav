local M = {}

-- WezTerm endpoint of `termnav paste-image`. The protocol is documented in
-- docs/reference.md and owned by src/paste_image/; keep these values in sync.
M.user_var = "TERMNAV_PASTE_IMAGE"
-- tmux matches this CSI-u sequence as user-keys[14].
M.trigger = "\27[777010u"

-- An arm lasts this long after Ctrl-V. A `ready` announcement after it, from
-- another pane, or a second one for the same arm sends nothing.
local arm_seconds = 10
-- Until a receiver acknowledges the header with the digest proof, only the
-- header (`<id>:<length>:<sha256>:`, digits, hex, and colons) is typed. A
-- forged `ready` therefore leaks no image data to whatever has focus.
local proof_length = 16
-- Bytes in flight after the first acknowledgement. The receiver acknowledges
-- every 128 KiB, so the window keeps the stream moving without letting a
-- focus change mid-transfer deliver more than this to another pane.
local window = 512 * 1024
-- Largest single send_text call; each runs from its own timer callback.
local chunk_bytes = 32 * 1024
-- Abandon the transfer when acknowledgements stop for this long.
local ack_timeout = 5

local function source_dir()
  if type(debug) == "table" and type(debug.getinfo) == "function" then
    local info = debug.getinfo(1, "S")
    local source = (info and info.source or ""):gsub("^@", ""):gsub("\\", "/")
    return source:match("^(.*)/[^/]+$")
  end
  return nil
end

-- Callers normally pass their link-routes instance as opts.routes so both
-- modules share one terminal-owner policy. This fallback serves consumers
-- that load the provider files directly from one directory.
local function load_routes(wezterm)
  local dir = source_dir()
  if not dir then
    return nil
  end
  for _, name in ipairs({ "link-routes.lua", "termnav-link-routes.lua" }) do
    local path = dir .. "/" .. name
    local file = io.open(path, "r")
    if file then
      file:close()
      return dofile(path).new(wezterm, {})
    end
  end
  return nil
end

function M.new(wezterm, opts)
  opts = opts or {}
  local routes = opts.routes or load_routes(wezterm)
  if not routes then
    error("termnav paste-image: pass the link-routes instance as opts.routes")
  end
  -- Collaborators are injectable for deterministic tests only.
  local clock = opts.clock or os.time
  local remove = opts.remove or os.remove
  local open_file = opts.open or io.open

  local paster = {}
  -- At most one armed pane and one transfer exist at a time.
  local armed = nil
  local transfer = nil
  local staging = false

  local function call_after(delay, callback)
    wezterm.time.call_after(delay, callback)
  end

  local function termnav()
    if type(opts.termnav) == "string" and opts.termnav ~= "" then
      return opts.termnav
    end
    if type(routes.helper_command) == "function" then
      return routes.helper_command("termnav")
    end
    return "termnav"
  end

  local function pane_id(pane)
    if type(routes.pane_id) == "function" then
      return routes.pane_id(pane)
    end
    local ok, id = pcall(function()
      return tostring(pane:pane_id())
    end)
    return ok and id or ""
  end

  local function disarm()
    if armed then
      remove(armed.path)
      armed = nil
    end
  end

  local function finish(state)
    if transfer == state then
      transfer = nil
    end
    if state.file then
      state.file:close()
      state.file = nil
    end
    remove(state.path)
  end

  local function send(state, data)
    local ok = pcall(function()
      state.pane:send_text(data)
    end)
    if not ok then
      -- The pane is gone; nothing more can be delivered to it.
      state.finished = true
      finish(state)
    end
    return ok
  end

  -- Stop sending and tell the receiver the stream ended, so it stops
  -- discarding input and nothing later lands in the pane.
  local function abandon(state)
    if transfer ~= state then
      return
    end
    if not state.finished then
      state.finished = true
      send(state, "!")
    end
    finish(state)
  end

  local function watch(state)
    local generation = state.ack_generation
    call_after(ack_timeout, function()
      if transfer == state and state.ack_generation == generation then
        abandon(state)
      end
    end)
  end

  local pump

  local function schedule(state)
    if not state.scheduled and not state.finished then
      state.scheduled = true
      call_after(0, function()
        pump(state)
      end)
    end
  end

  -- Send one bounded chunk, then yield to WezTerm's event loop before the
  -- next one. Never called from the user-var event itself.
  pump = function(state)
    state.scheduled = false
    if transfer ~= state or state.finished then
      return
    end
    if not state.file then
      state.file = open_file(state.path, "rb")
      local head = state.file and state.file:read(96)
      local length, digest = (head or ""):match("^([1-9]%d*):([0-9a-f]+):")
      if not length or #digest ~= 64 or not state.file:seek("set", #length + 66) then
        abandon(state)
        return
      end
      state.prefix = state.id .. ":" .. length .. ":" .. digest .. ":"
      state.proof = digest:sub(1, proof_length)
    end
    if not state.acked then
      -- Before a proven acknowledgement only the header may be typed.
      if state.prefix then
        local header = state.prefix
        state.prefix = nil
        if send(state, header) then
          state.sent = state.sent + #header
        end
      end
      return
    end
    local budget = math.min(chunk_bytes, state.acked + window - state.sent)
    if budget <= 0 then
      -- Wait for an acknowledgement; ack handling reschedules.
      return
    end
    local wanted = budget
    local piece = state.file:read(wanted) or ""
    -- Only base64 characters follow the header, even if the staged file was
    -- replaced.
    if piece:find("[^%w+/=]") then
      abandon(state)
      return
    end
    local data = piece
    -- A short read means end of file inside this chunk's budget, so the
    -- terminator still fits.
    if #piece < wanted then
      data = data .. "!"
      state.finished = true
    end
    if not send(state, data) then
      return
    end
    state.sent = state.sent + #data
    if state.finished then
      state.file:close()
      state.file = nil
      -- Keep the transfer until `done` or `cancel` so the staged file is
      -- deleted on the receiver's verdict; the ack watch still bounds it.
      return
    end
    schedule(state)
  end

  local function start(pane, id)
    local arm = armed
    if not arm or transfer or arm.pane_id ~= pane_id(pane) or clock() >= arm.expires then
      return
    end
    armed = nil
    transfer = {
      pane = pane,
      id = id,
      path = arm.path,
      prefix = nil,
      proof = nil,
      sent = 0,
      acked = nil,
      ack_generation = 0,
      finished = false,
      scheduled = false,
    }
    watch(transfer)
    schedule(transfer)
  end

  local function arm(pane, id, path)
    disarm()
    local state = { pane_id = id, path = path, expires = clock() + arm_seconds }
    armed = state
    call_after(arm_seconds, function()
      if armed == state then
        armed = nil
        remove(state.path)
      end
    end)
    local ok = pcall(function()
      pane:send_text(M.trigger)
    end)
    if not ok and armed == state then
      disarm()
    end
  end

  -- Paste text exactly as before, then, for tmux panes only, stage the
  -- clipboard image in a scheduled callback and ask tmux for a receiver.
  function paster:paste(window_, pane)
    window_:perform_action(wezterm.action.PasteFrom("Clipboard"), pane)
    if staging or transfer then
      return
    end
    if not wezterm.time or type(wezterm.time.call_after) ~= "function" then
      return
    end
    local owner_ok, owner = pcall(routes.terminal_owner, pane)
    if not owner_ok or owner ~= "tmux" then
      return
    end
    local id = pane_id(pane)
    if id == "" then
      return
    end
    staging = true
    call_after(0, function()
      local ok, succeeded, stdout =
        pcall(wezterm.run_child_process, { termnav(), "paste-image", "stage" })
      staging = false
      local path = ok
        and succeeded
        and type(stdout) == "string"
        and stdout:match("^(/[^\r\n]+)\r?\n?$")
      if not path then
        return
      end
      if transfer then
        remove(path)
        return
      end
      arm(pane, id, path)
    end)
  end

  function paster:paste_action()
    return wezterm.action_callback(function(window_, pane)
      paster:paste(window_, pane)
    end)
  end

  -- Handle one user-var-changed event. Returns true when the variable belongs
  -- to this protocol, whether or not the announcement was acted on.
  function paster:user_var_changed(_, pane, name, value)
    if name ~= M.user_var then
      return false
    end
    if type(value) ~= "string" then
      return true
    end
    local kind, id, rest = value:match("^(%l+):([0-9a-f]+)(.*)$")
    if not kind or #id < 8 or #id > 64 then
      return true
    end
    if kind == "ready" then
      if rest == "" then
        start(pane, id)
      end
      return true
    end
    local state = transfer
    if not state or pane_id(pane) ~= pane_id(state.pane) then
      return true
    end
    if kind == "cancel" and rest == "" then
      -- Any receiver in this pane's tmux rejecting input ends the transfer,
      -- even under another id: a forged `ready` may have won the race, and
      -- the real receiver is discarding until the terminator.
      abandon(state)
      return true
    end
    if state.id ~= id then
      return true
    end
    if kind == "ack" then
      local digits, proof = rest:match("^:(%d+):([0-9a-f]+)$")
      local offset = tonumber(digits or "")
      if
        offset
        and proof == state.proof
        and offset <= state.sent
        and offset > (state.acked or -1)
      then
        state.acked = offset
        state.ack_generation = state.ack_generation + 1
        watch(state)
        schedule(state)
      end
    elseif kind == "done" and rest == "" then
      finish(state)
    end
    return true
  end

  return paster
end

return M
