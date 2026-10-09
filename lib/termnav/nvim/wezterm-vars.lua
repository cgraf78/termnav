-- Publish WezTerm user vars from inside nvim.
--
-- nvim's TUI path cannot use plain io.write reliably, so callers write OSC
-- directly to the pane tty. Inside tmux, that OSC must be wrapped in DCS
-- passthrough so it reaches the outer WezTerm process. Outside tmux the
-- editor names its own terminal; see editor_tty_path().

-- selene: allow(undefined_variable)
local vim = vim

local M = {}

local tty_path
-- hrtime before which publishing is skipped after a write timed out, so a
-- stuck terminal costs one bounded wait per second rather than one per
-- user var on every autocmd.
local stalled_until = 0
local stall_cooldown_ns = 1000 * 1000000
local base64_alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

local function base64_encode(value)
  if vim.base64 and type(vim.base64.encode) == "function" then
    return vim.base64.encode(value)
  end

  -- Neovim 0.9 ships on Ubuntu 24.04 and does not provide vim.base64 yet.
  -- Keep this module self-contained so publishing user vars works on distro
  -- Neovim builds without shelling out during every focus/update event.
  return (
    (value:gsub(".", function(char)
      local byte = char:byte()
      local bits = ""
      for shift = 7, 0, -1 do
        bits = bits .. (math.floor(byte / 2 ^ shift) % 2 == 1 and "1" or "0")
      end
      return bits
    end) .. "0000"):gsub("%d%d%d?%d?%d?%d?", function(bits)
      if #bits < 6 then
        return ""
      end
      local index = 0
      for bit = 1, 6 do
        index = index + (bits:sub(bit, bit) == "1" and 2 ^ (6 - bit) or 0)
      end
      return base64_alphabet:sub(index + 1, index + 1)
    end) .. ({ "", "==", "=" })[#value % 3 + 1]
  )
end

local function tmux_tty_path()
  local pane = vim.env.TMUX_PANE
  if type(pane) ~= "string" or pane == "" then
    return nil
  end

  local output = vim.fn.system({ "tmux", "display-message", "-t", pane, "-p", "#{pane_tty}" })
  if vim.v.shell_error ~= 0 then
    return nil
  end

  output = output:gsub("%s+$", "")
  if output == "" then
    return nil
  end

  return output
end

local function tmux_client_termname()
  local pane = vim.env.TMUX_PANE
  if type(pane) ~= "string" or pane == "" then
    return nil
  end

  local output =
    vim.fn.system({ "tmux", "display-message", "-t", pane, "-p", "#{client_termname}" })
  if vim.v.shell_error ~= 0 then
    return nil
  end

  output = output:gsub("%s+$", "")
  if output == "" then
    return nil
  end

  return output
end

local function tmux_client_is_nested(batch)
  local tmux = vim.env.TMUX
  local pane = vim.env.TMUX_PANE
  if type(tmux) ~= "string" or tmux == "" or type(pane) ~= "string" or pane == "" then
    return false, false
  end

  local key = tmux .. "\0" .. pane
  if type(batch) == "table" and batch.tmux_client_key == key then
    local nested = batch.tmux_client_nested
    return nested == true, type(nested) == "boolean"
  end

  local termname = tmux_client_termname()
  if type(batch) == "table" then
    -- Unknown is shared only for this synchronous batch. A later publish gets a
    -- fresh table and retries instead of carrying uncertain topology forward.
    batch.tmux_client_key = key
    batch.tmux_client_nested = nil
  end
  if type(termname) ~= "string" then
    return false, false
  end

  local nested = termname:match("^tmux") ~= nil or termname:match("^screen") ~= nil
  -- A batch exists only for one synchronous setup publish. Sharing a successful
  -- observation there avoids repeated subprocesses without carrying attachment
  -- topology across focus, detach, or resume boundaries.
  if type(batch) == "table" then
    batch.tmux_client_nested = nested
  end
  return nested, true
end

-- True while a terminal UI attached over the editor's stdio is present: the
-- TUI that started this editor process and shares its stderr. A UI attached
-- over a socket (`--remote-ui`, or the one left after `:detach`) can sit in a
-- different terminal, and the stderr device would then be the wrong pane.
local function stdio_tui_attached()
  local ok, uis = pcall(vim.api.nvim_list_uis)
  if not ok or type(uis) ~= "table" then
    return false
  end
  for _, ui in ipairs(uis) do
    if ui.stdout_tty and ui.chan then
      local ok_info, info = pcall(vim.api.nvim_get_chan_info, ui.chan)
      if ok_info and type(info) == "table" and info.stream == "stdio" then
        return true
      end
    end
  end
  return false
end

-- Neovim 0.10+ runs the editor in its own session, separate from the TUI, so
-- opening /dev/tty fails with ENXIO even though the editor's stderr is still
-- the terminal. Name that device directly: procfs on Linux, ttyname(3) through
-- LuaJIT's FFI elsewhere (macOS has no procfs). Returns nil unless the TUI
-- that owns stderr is attached, when stderr is not a terminal, or when
-- neither method applies, e.g. a PUC Lua build off Linux.
function M.stderr_tty_path()
  local uv = vim.uv or vim.loop
  if not uv or uv.guess_handle(2) ~= "tty" or not stdio_tui_attached() then
    return nil
  end
  local path = uv.fs_readlink("/proc/self/fd/2")
  if type(path) == "string" and path:match("^/dev/") then
    return path
  end
  local ok, name = pcall(function()
    local ffi = require("ffi")
    pcall(ffi.cdef, "char *ttyname(int fd);")
    local pointer = ffi.C.ttyname(2)
    return pointer ~= nil and ffi.string(pointer) or nil
  end)
  return ok and name or nil
end

-- Terminal for writes from the editor when no tmux pane sits in between.
-- The controlling terminal keeps priority while the editor has one (Neovim
-- before 0.10): unlike the device path, /dev/tty also works for a user who
-- may not reopen that pty by name, e.g. under `su`. Otherwise the TUI's
-- stderr terminal; nil when neither is available. navigation.lua passes the
-- same answer to the native requests it starts.
function M.editor_tty_path()
  local probe = io.open("/dev/tty", "w")
  if probe then
    probe:close()
    return "/dev/tty"
  end
  return M.stderr_tty_path()
end

-- Open a terminal for one write without ever blocking the editor.
-- O_NONBLOCK keeps open() from waiting for a carrier that may not return
-- (BSD and macOS ptys whose terminal has closed) and write() from waiting on
-- a terminal that stopped reading. A queue that is only momentarily full
-- (BSD ptys hold about 1 KiB, so a TUI redraw can fill one) gets a short
-- bounded retry, which also makes leaving half an escape sequence in the
-- stream unlikely; past that the write reports failure and setup retries
-- later.
-- O_NOCTTY keeps the editor, a session leader without a controlling terminal
-- on Neovim 0.10+, from adopting the pty. Returns a handle with the
-- write/flush/close subset of an io file, where close() reports whether
-- everything was written, or nil when the open failed.
function M.open_tty(path)
  local uv = vim.uv or vim.loop
  local flags = uv and uv.constants
  -- LuaJIT ships `bit`; Neovim bundles it for PUC Lua builds.
  local has_bit, bit = pcall(require, "bit")
  if not (flags and flags.O_WRONLY and flags.O_NONBLOCK and flags.O_NOCTTY and has_bit) then
    return io.open(path, "w")
  end
  local fd = uv.fs_open(path, bit.bor(flags.O_WRONLY, flags.O_NONBLOCK, flags.O_NOCTTY), 0)
  if not fd then
    return nil
  end
  local complete = true
  return {
    write = function(_, data)
      local deadline = uv.hrtime() + 100 * 1000000
      while complete and #data > 0 do
        local written, _, code = uv.fs_write(fd, data, -1)
        if written and written > 0 then
          data = data:sub(written + 1)
        elseif (written or code == "EAGAIN") and uv.hrtime() < deadline then
          uv.sleep(1)
        else
          complete = false
        end
      end
    end,
    flush = function() end,
    close = function()
      uv.fs_close(fd)
      return complete
    end,
  }
end

local function tmux_passthrough(sequence)
  return "\027Ptmux;" .. sequence:gsub("\027", "\027\027") .. "\027\\"
end

function M.tty_path()
  if not vim.env.TMUX then
    -- Not cached: UIs can attach and detach during the editor's lifetime.
    return M.editor_tty_path()
  end

  if type(tty_path) == "string" and tty_path ~= "" then
    return tty_path
  end

  local path = tmux_tty_path()
  -- Do not cache a failed lookup. Startup timing can briefly make tmux pane
  -- metadata unavailable, and later focus events should be able to recover.
  if not path then
    return nil
  end
  tty_path = path
  return tty_path
end

function M.set(name, value, batch)
  local uv = vim.uv or vim.loop
  if uv and uv.hrtime() < stalled_until then
    return false
  end
  local path = M.tty_path()
  if not path then
    return false
  end

  local encoded = base64_encode(value or "")
  local osc = ("\027]1337;SetUserVar=%s=%s\007"):format(name, encoded)
  if vim.env.TMUX then
    local nested, known = tmux_client_is_nested(batch)
    -- Without a client classification, the required passthrough depth is
    -- unknown. Report failure before writing so setup retries this variable.
    if not known then
      return false
    end
    osc = tmux_passthrough(osc)
    if nested then
      osc = tmux_passthrough(osc)
    end
  end

  local tty = M.open_tty(path)
  if not tty then
    return false
  end

  tty:write(osc)
  tty:flush()
  -- io files and test doubles return true or nil; only a short write fails.
  if tty:close() == false then
    if uv then
      stalled_until = uv.hrtime() + stall_cooldown_ns
    end
    return false
  end
  return true
end

return M
