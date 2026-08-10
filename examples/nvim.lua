-- Load Termnav's Neovim setup without pinning an installation directory.
--
-- The provider module owns socket publication, focus leases, and terminal
-- context. The consuming config can override its events or collaborators via
-- `termnav_options`, but it should not copy those protocol details.

local M = {}

local function required(value, name)
  if type(value) ~= "string" or value == "" then
    error("termnav Neovim example requires options." .. name, 3)
  end
  return value
end

local function shdeps_api(options)
  if type(options.shdeps) == "table" then
    return options.shdeps
  end

  local home = options.home or os.getenv("HOME")
  local lua_dir = options.shdeps_lua_dir or os.getenv("SHDEPS_LUA_DIR")
  if not lua_dir then
    lua_dir = required(home, "home") .. "/.local/lib/shdeps"
  end

  local bootstrap = dofile(lua_dir .. "/shdeps/bootstrap.lua")
  return bootstrap.new({
    home = home,
    conf_dir = options.shdeps_conf_dir,
    bin = options.shdeps_bin,
    bin_dir = options.shdeps_bin_dir,
    root = options.shdeps_root,
    env = options.shdeps_env,
  })
end

function M.setup(options)
  options = options or {}
  local api = shdeps_api(options)
  local path = api.dep_file("cgraf78/termnav", "lib/termnav/nvim/setup.lua")
  if not path then
    error("Shdeps could not resolve Termnav's Neovim setup module", 2)
  end

  return dofile(path).setup(options.termnav_options)
end

return M
