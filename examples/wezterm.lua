-- Compose Termnav's reusable routes into an existing WezTerm configuration.
--
-- The consumer still owns fonts, colors, domains, key assignments, and the
-- rest of terminal policy. Termnav owns only the route parser and the public
-- token rules shared with its tmux Ctrl-click implementation.

local M = {}

local function required(value, name)
  if type(value) ~= "string" or value == "" then
    error("termnav WezTerm example requires options." .. name, 3)
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

  -- Ask Shdeps for provider assets rather than assuming Termnav is a source
  -- checkout. This keeps the same config valid for repo and release installs.
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

local function dependency_module(api, relative_path)
  local path = api.dep_file("cgraf78/termnav", relative_path)
  if not path then
    error("Shdeps could not resolve Termnav asset " .. relative_path, 3)
  end
  return dofile(path)
end

function M.apply(options)
  options = options or {}
  local wezterm = options.wezterm or require("wezterm")
  local config = options.config
    or (type(wezterm.config_builder) == "function" and wezterm.config_builder() or {})
  local api = shdeps_api(options)

  local routes = dependency_module(api, "lib/termnav/wezterm/link-routes.lua").new(wezterm)
  local public_rules = dependency_module(api, "lib/termnav/wezterm/public-link-rules.lua")

  -- Preserve caller rules and append fresh Termnav-owned copies. A caller may
  -- reorder the result afterward without mutating the provider's definitions.
  local rules = config.hyperlink_rules
  if type(rules) ~= "table" then
    rules = type(wezterm.default_hyperlink_rules) == "function"
        and wezterm.default_hyperlink_rules()
      or {}
  end
  config.hyperlink_rules = public_rules.add_public_link_rules(rules)
  routes.setup()

  return {
    config = config,
    routes = routes,
  }
end

return M
