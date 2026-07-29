-- Publish WezTerm user vars from inside nvim.
--
-- nvim's TUI path cannot use plain io.write reliably, so callers write OSC
-- directly to the pane tty. Inside tmux, that OSC must be wrapped in DCS
-- passthrough so it reaches the outer WezTerm process.

-- selene: allow(undefined_variable)
local vim = vim

local M = {}

local tty_path
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

local function tmux_passthrough(sequence)
  return "\027Ptmux;" .. sequence:gsub("\027", "\027\027") .. "\027\\"
end

function M.tty_path()
  if type(tty_path) == "string" and tty_path ~= "" then
    return tty_path
  end

  if vim.env.TMUX then
    local path = tmux_tty_path()
    -- Do not cache a failed lookup. Startup timing can briefly make tmux pane
    -- metadata unavailable, and later focus events should be able to recover.
    if not path then
      return nil
    end
    tty_path = path
  else
    tty_path = "/dev/tty"
  end

  return tty_path
end

function M.set(name, value, batch)
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

  local tty = io.open(path, "w")
  if not tty then
    return false
  end

  tty:write(osc)
  tty:flush()
  tty:close()
  return true
end

return M
