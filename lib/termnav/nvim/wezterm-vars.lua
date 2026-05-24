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

function M.set(name, value)
  local path = M.tty_path()
  if not path then
    return false
  end

  local encoded = base64_encode(value or "")
  local osc
  if vim.env.TMUX then
    osc = ("\027Ptmux;\027\027]1337;SetUserVar=%s=%s\007\027\\"):format(name, encoded)
  else
    osc = ("\027]1337;SetUserVar=%s=%s\007"):format(name, encoded)
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
