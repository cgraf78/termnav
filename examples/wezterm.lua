-- Compose Termnav's reusable routes into an existing WezTerm configuration.
--
-- The consumer still owns fonts, colors, domains, key assignments, and the
-- rest of terminal policy. Termnav owns only the route parser and the public
-- token rules shared with its tmux Ctrl-click implementation.

local M = {}

local function asset_path(options, wezterm, relative_path)
  if type(options.asset_path) == "function" then
    return options.asset_path(relative_path)
  end
  local command = options.termnav_command or "termnav"
  -- WezTerm's argv-form child API preserves executable and argument boundaries
  -- without involving a shell. This matters for managed installation paths
  -- containing spaces, quotes, dollar signs, or command-substitution tokens.
  local ok, stdout = wezterm.run_child_process({ command, "asset-path", relative_path })
  local path = ok and stdout and stdout:match("([^\r\n]+)") or nil
  if not path or path == "" then
    error("termnav could not resolve asset " .. relative_path, 3)
  end
  return path
end

function M.apply(options)
  options = options or {}
  local wezterm = options.wezterm or require("wezterm")
  local config = options.config
    or (type(wezterm.config_builder) == "function" and wezterm.config_builder() or {})
  local routes =
    dofile(asset_path(options, wezterm, "lib/termnav/wezterm/link-routes.lua")).new(wezterm)
  local public_rules =
    dofile(asset_path(options, wezterm, "lib/termnav/wezterm/public-link-rules.lua"))

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
