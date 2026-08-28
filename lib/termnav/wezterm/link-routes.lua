local M = {}

-- Read side of termnav's private WezTerm user-var protocol. Consumers should
-- call route helpers such as is_nvim() instead of reading the raw pane vars.
-- The nvim writer in lib/termnav/nvim/setup.lua declares the same names; the two
-- run in different Lua hosts and cannot share a module reliably, so keep the two
-- tables in sync.
local user_vars = {
  is_nvim = "IS_NVIM",
  open_socket = "NVIM_OPEN_SOCKET",
  link_cwd = "NVIM_LINK_CWD",
  remote_link_host = "NVIM_REMOTE_LINK_HOST",
  remote_cwd = "NVIM_REMOTE_CWD",
  remote_tmux = "NVIM_REMOTE_TMUX",
}

-- Shell-only state is separate from the Neovim protocol table above: Neovim
-- does not own the surrounding shell's tmux membership and must never publish
-- it. Consumers still read this state only through route helpers.
local shell_user_vars = {
  tmux = "TERMNAV_TMUX",
}

function M.new(wezterm, options)
  options = options or {}
  local routes = {}
  -- Pane metadata belongs to the child process and can be shared by several
  -- terminal clients attached to one tmux session. Instance identity must
  -- instead come from the WezTerm process handling this callback. An explicit
  -- setup value is easiest for named GUI classes; the environment fallbacks
  -- support mux sockets injected into that GUI's launch environment.
  local remote_open_scope = options.remote_open_scope
    or os.getenv("TERMNAV_WEZTERM_SCOPE")
    or os.getenv("WEZTERM_UNIX_SOCKET")
    or ""

  function routes.uri_decode(s)
    return (s:gsub("%%(%x%x)", function(hex)
      return string.char(tonumber(hex, 16))
    end))
  end

  function routes.local_file_host(host)
    host = host:lower()
    if host == "" or host == "localhost" or host == "127.0.0.1" or host == "::1" then
      return true
    end

    local hostname = wezterm.hostname():lower()
    return host == hostname or host == hostname .. ".local" or host == hostname .. ".lan"
  end

  function routes.is_nvim(pane)
    return pane:get_user_vars()[user_vars.is_nvim] == "true"
  end

  local function tmux_owner(pane, pane_user_vars)
    local shell_tmux = pane_user_vars[shell_user_vars.tmux]
    if shell_tmux == "true" then
      -- Positive metadata is authoritative and keeps managed tmux panes off
      -- WezTerm's foreground-process query on every navigation gesture.
      return true
    end
    if shell_tmux ~= nil then
      -- An empty shell value clears stale remote metadata after a child exits,
      -- but it cannot describe an unmanaged tmux process launched afterward.
      -- Retain the compatibility probe so that child still owns its chords.
      return routes.foreground_basename(pane) == "tmux"
    end

    if pane_user_vars[user_vars.remote_tmux] == "true" then
      return true
    end

    -- Metadata-free panes retain the compatibility fallback for unmanaged
    -- tmux clients. This is intentionally the only path that asks WezTerm to
    -- inspect the foreground process.
    return routes.foreground_basename(pane) == "tmux"
  end

  function routes.is_tmux(pane)
    if not pane or type(pane.get_user_vars) ~= "function" then
      return false
    end

    return tmux_owner(pane, pane:get_user_vars() or {})
  end

  function routes.terminal_owner(pane)
    if not pane or type(pane.get_user_vars) ~= "function" then
      return "terminal"
    end

    -- Navigation callbacks need one mutually exclusive answer. Snapshot the
    -- protocol once so a single UI gesture cannot perform duplicate RPC-like
    -- user-var reads or observe two different publication generations.
    local pane_user_vars = pane:get_user_vars() or {}
    if pane_user_vars[user_vars.is_nvim] == "true" then
      return "nvim"
    end
    if tmux_owner(pane, pane_user_vars) then
      return "tmux"
    end
    return "terminal"
  end

  function routes.file_uri_path(uri)
    local host, path = uri:match("^file://([^/]*)(/.*)$")
    if not path then
      return nil
    end

    if not routes.local_file_host(host) then
      return nil
    end

    return routes.uri_decode(path)
  end

  function routes.remote_file_uri(uri)
    local host, path = uri:match("^file://([^/]+)(/.*)$")
    if not path or routes.local_file_host(host) then
      return nil, nil
    end

    return host, routes.uri_decode(path)
  end

  function routes.remote_uri(uri)
    local host, path = uri:match("^nvim%-remote://([^/]+)(/.*)$")
    if not path or host == "" then
      return nil, nil
    end

    return host, routes.uri_decode(path)
  end

  function routes.pane_cwd_is_local(pane)
    local cwd_uri = pane:get_current_working_dir()
    if not cwd_uri then
      return true
    end

    local host = cwd_uri.host or ""
    return routes.local_file_host(host)
  end

  function routes.pane_link_cwd(pane)
    local user_var_cwd = pane:get_user_vars()[user_vars.link_cwd]
    if user_var_cwd and user_var_cwd ~= "" then
      return user_var_cwd
    end

    local cwd_uri = pane:get_current_working_dir()
    return cwd_uri and cwd_uri.file_path or ""
  end

  function routes.remote_pane_info(pane)
    local pane_user_vars = pane:get_user_vars()
    local user_var_host = pane_user_vars[user_vars.remote_link_host]
    local user_var_cwd = pane_user_vars[user_vars.remote_cwd]
    local cwd_uri = pane:get_current_working_dir()
    if user_var_host and user_var_host ~= "" and not routes.local_file_host(user_var_host) then
      -- The shell/nvim-published cwd is authoritative. WezTerm's cwd metadata
      -- can point at the local ssh process or be stale inside tmux, so only use
      -- it when the host explicitly matches the remote pane.
      if user_var_cwd and user_var_cwd ~= "" then
        return user_var_host, user_var_cwd
      end
      if cwd_uri and cwd_uri.host == user_var_host then
        return user_var_host, cwd_uri.file_path
      end
      return user_var_host, nil
    end

    if not cwd_uri then
      return nil, nil
    end

    local host = cwd_uri.host or ""
    if host == "" or routes.local_file_host(host) then
      return nil, nil
    end

    return host, cwd_uri.file_path
  end

  function routes.relative_path_info(path_info)
    local home = path_info == "~"
      or path_info:sub(1, 2) == "~/"
      or path_info == "$HOME"
      or path_info:sub(1, 6) == "$HOME/"
      or path_info == "${HOME}"
      or path_info:sub(1, 8) == "${HOME}/"

    return not path_info:match("^/") and not path_info:match("^%a:[/\\]") and not home
  end

  function routes.resolve_remote_path_info(path_info, cwd)
    if cwd and cwd ~= "" and routes.relative_path_info(path_info) then
      return cwd:gsub("/$", "") .. "/" .. path_info
    end

    return path_info
  end

  local function basename(path)
    return tostring(path or ""):gsub("\\", "/"):match("([^/]+)$") or ""
  end

  function routes.foreground_basename(pane)
    if not pane or type(pane.get_foreground_process_name) ~= "function" then
      return nil
    end

    local ok, name = pcall(function()
      return pane:get_foreground_process_name()
    end)
    if not ok then
      return nil
    end
    return basename(name)
  end

  function routes.helper_command(name)
    local bin_dir = os.getenv("TERMNAV_BIN_DIR")
    if bin_dir and bin_dir ~= "" then
      return bin_dir:gsub("/$", "") .. "/" .. name
    end
    return name
  end

  function routes.run_nvim_helper(_, argv)
    -- WezTerm invokes open-uri on its GUI event loop. Never wait there for SSH,
    -- Neovim RPC, or a consumer transport: the native helper owns their hard
    -- deadlines and cleanup, while this callback only acknowledges successful
    -- process creation.
    wezterm.background_child_process(argv)
    return true
  end

  function routes.pane_id(pane)
    if pane and type(pane.pane_id) == "function" then
      return tostring(pane:pane_id())
    end
    return ""
  end

  function routes.pane_scope(_)
    return remote_open_scope
  end

  function routes.open_in_nvim(window, pane, path_info)
    routes.run_nvim_helper(window, {
      routes.helper_command("termnav"),
      "nvim",
      "open",
      "link",
      path_info,
      routes.pane_link_cwd(pane),
      routes.is_nvim(pane) and "nvim" or "terminal",
      pane:get_user_vars()[user_vars.open_socket] or "",
    }, "No nvim session found for file link: " .. path_info)
  end

  function routes.open_remote_in_nvim(window, pane, remote_host, path_info)
    -- The native opener first reuses an authenticated ControlMaster and then,
    -- if configured, asks one explicit transport helper to act on this exact
    -- pane. A raw terminal byte stream cannot acknowledge tmux command mode, so
    -- this module deliberately never synthesizes prefix keys or timed typing.
    routes.run_nvim_helper(window, {
      routes.helper_command("termnav"),
      "nvim",
      "open",
      "link",
      path_info,
      "",
      "remote",
      remote_host,
      "wezterm",
      routes.pane_scope(pane),
      routes.pane_id(pane),
    }, "No nvim session found for " .. remote_host .. ": " .. path_info)
  end

  function routes.open_uri(window, pane, uri)
    local path_info = uri:match("^nvim%-open://(.+)$")
    if path_info then
      path_info = routes.uri_decode(path_info)
    end
    local lazygit_path_info = uri:match("^lazygit%-edit://(.+)$")
    if lazygit_path_info then
      path_info = routes.uri_decode(lazygit_path_info)
    end
    if path_info then
      local remote_host, remote_cwd = routes.remote_pane_info(pane)
      if remote_host then
        routes.open_remote_in_nvim(
          window,
          pane,
          remote_host,
          routes.resolve_remote_path_info(path_info, remote_cwd)
        )
        return false
      end

      routes.open_in_nvim(window, pane, path_info)
      return false
    end

    local remote_host
    remote_host, path_info = routes.remote_uri(uri)
    if path_info then
      routes.open_remote_in_nvim(window, pane, remote_host, path_info)
      return false
    end

    remote_host, path_info = routes.remote_file_uri(uri)
    if path_info then
      routes.open_remote_in_nvim(window, pane, remote_host, path_info)
      return false
    end

    path_info = routes.file_uri_path(uri)
    if path_info then
      if routes.pane_cwd_is_local(pane) then
        routes.open_in_nvim(window, pane, path_info)
        return false
      end
      return true
    end

    return true
  end

  function routes.setup()
    wezterm.on("open-uri", function(window, pane, uri)
      return routes.open_uri(window, pane, uri)
    end)
    wezterm.on("user-var-changed", function(window, pane, name, value)
      if value == "" then
        return
      end

      -- Rust appends a nonce to navigation values so repeated gestures still
      -- produce distinct user-var updates. The direction before the colon is
      -- the stable public vocabulary consumed here.
      local direction = value:match("^([^:]+):") or value
      if name == "TERMNAV_TAB_SELECT" and (direction == "next" or direction == "previous") then
        local relative = direction == "previous" and -1 or 1
        window:perform_action(wezterm.action.ActivateTabRelative(relative), pane)
      elseif name == "TERMNAV_TAB_MOVE" and (direction == "left" or direction == "right") then
        local relative = direction == "left" and -1 or 1
        window:perform_action(wezterm.action.MoveTabRelative(relative), pane)
      elseif name == "TERMNAV_OPEN_URL" then
        wezterm.open_with(value)
      end
    end)
  end

  return routes
end

return M
