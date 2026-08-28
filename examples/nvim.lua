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
  local pipe = assert(io.popen(string.format("%q asset-path %q", command, relative_path), "r"))
  local path = pipe:read("*l")
  local ok = pipe:close()
  if not ok or not path or path == "" then
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
