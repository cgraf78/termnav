-- selene: allow(undefined_variable)
local vim = vim

-- Load Termnav's Neovim setup without pinning an installation directory.
--
-- The provider module owns socket publication, focus leases, and terminal
-- context. The consuming config can override its events or collaborators via
-- `termnav_options`, but it should not copy those protocol details.

local M = {}

local function asset_path(options, relative_path)
  if type(options.asset_path) == "function" then
    return options.asset_path(relative_path)
  end
  local command = options.termnav_command or "termnav"
  -- List-form system() bypasses the shell entirely. Configuration paths often
  -- contain spaces or shell metacharacters, and an example intended for
  -- copy/paste must not turn either the executable path or asset name into
  -- shell syntax.
  local output = vim.fn.systemlist({ command, "asset-path", relative_path })
  local path = output[1]
  if vim.v.shell_error ~= 0 or not path or path == "" then
    error("termnav could not resolve asset " .. relative_path, 2)
  end
  return path
end

function M.setup(options)
  options = options or {}
  local path = asset_path(options, "lib/termnav/nvim/setup.lua")

  return dofile(path).setup(options.termnav_options)
end

return M
