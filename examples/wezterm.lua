-- Compose Termnav's reusable routes into an existing WezTerm configuration.
--
-- The consumer still owns fonts, colors, domains, key assignments, and the
-- rest of terminal policy. Termnav owns only the route parser and the public
-- token rules shared with its tmux Ctrl-click implementation.

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
    error("termnav could not resolve asset " .. relative_path, 3)
  end
  return path
end

function M.apply(options)
  options = options or {}
  local wezterm = options.wezterm or require("wezterm")
  local config = options.config
    or (type(wezterm.config_builder) == "function" and wezterm.config_builder() or {})
  local routes = dofile(asset_path(options, "lib/termnav/wezterm/link-routes.lua")).new(wezterm)
  local public_rules = dofile(asset_path(options, "lib/termnav/wezterm/public-link-rules.lua"))

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
