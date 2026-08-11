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

function M.new(wezterm)
  local routes = {}

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

  function routes.is_tmux(pane)
    if not pane or type(pane.get_user_vars) ~= "function" then
      return false
    end

    local pane_user_vars = pane:get_user_vars() or {}
    if routes.foreground_basename(pane) == "tmux" then
      return true
    end

    local shell_tmux = pane_user_vars[shell_user_vars.tmux]
    if shell_tmux ~= nil then
      -- The current shell reasserts both true and empty values after a child
      -- exits. An explicit empty value must therefore win over remote metadata
      -- left by the child process that previously controlled this pane.
      return shell_tmux == "true"
    end

    return routes.remote_tmux_pane(pane)
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

  function routes.shell_quote(value)
    return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
  end

  function routes.tmux_double_quote(value)
    return '"' .. tostring(value):gsub("\\", "\\\\"):gsub('"', '\\"') .. '"'
  end

  function routes.helper_command(name)
    local bin_dir = os.getenv("TERMNAV_BIN_DIR")
    if bin_dir and bin_dir ~= "" then
      return bin_dir:gsub("/$", "") .. "/" .. name
    end
    return name
  end

  function routes.remote_path_command()
    return 'PATH="$PATH:$HOME/.local/bin:$HOME/.local/share/mise/shims:'
      .. '/opt/homebrew/bin:/usr/local/bin"; export PATH'
  end

  function routes.remote_tmux_open_command(path_info)
    local shell_command = routes.remote_path_command()
      .. "; command nvim-tmux-open tmux-link "
      .. routes.shell_quote(path_info)
    return "run-shell " .. routes.tmux_double_quote(shell_command)
  end

  function routes.show_open_error(window, message)
    if window and type(window.toast_notification) == "function" then
      pcall(function()
        window:toast_notification("nvim open", message, nil, 4000)
      end)
    end
    if type(wezterm.log_error) == "function" then
      wezterm.log_error(message)
    end
  end

  function routes.run_nvim_helper(window, argv, failure_message)
    -- open-uri handlers normally lose background stderr. When WezTerm gives us
    -- a window, run the helper synchronously so users get a toast with the
    -- actual opener error. Headless/test contexts keep the background path.
    if window and type(wezterm.run_child_process) == "function" then
      local ok, stdout, stderr = wezterm.run_child_process(argv)
      if ok == true then
        return true
      end

      local detail = tostring(stderr or stdout or ""):match("[^\r\n]+")
      routes.show_open_error(window, detail or failure_message)
      return false
    end

    wezterm.background_child_process(argv)
    return true
  end

  function routes.remote_tmux_pane(pane)
    if not pane or type(pane.get_user_vars) ~= "function" then
      return false
    end
    return pane:get_user_vars()[user_vars.remote_tmux] == "true"
  end

  function routes.open_remote_via_controlmaster(remote_host, path_info)
    if type(wezterm.run_child_process) ~= "function" then
      return false
    end

    local ok = wezterm.run_child_process({
      routes.helper_command("nvim-ssh-control-open"),
      remote_host,
      path_info,
    })
    return ok == true
  end

  function routes.send_remote_tmux_open(pane, path_info)
    if
      not pane
      or type(pane.send_text) ~= "function"
      or not wezterm.time
      or type(wezterm.time.call_after) ~= "function"
    then
      return false
    end

    local foreground = routes.foreground_basename(pane)
    if
      not routes.remote_tmux_pane(pane)
      or not foreground
      or foreground == ""
      or foreground == "tmux"
    then
      return false
    end

    -- Without a local tmux client, there is no local pane for the helper to
    -- target. Send the command to the remote tmux client already visible in
    -- this WezTerm pane. The producing shell/nvim must publish that the remote
    -- side is actually inside tmux; foreground process names alone only tell us
    -- this is not local tmux. tmux needs a tick to enter command mode after the
    -- prefix; sending the whole sequence at once can leave `run-shell` at the
    -- shell prompt instead of in tmux's command prompt.
    pane:send_text("\002:")
    wezterm.time.call_after(0.15, function()
      -- The pane may have closed while the timer was pending.
      pcall(function()
        pane:send_text(routes.remote_tmux_open_command(path_info) .. "\r")
      end)
    end)
    return true
  end

  function routes.open_in_nvim(window, pane, path_info)
    routes.run_nvim_helper(window, {
      routes.helper_command("nvim-tmux-open"),
      "link",
      path_info,
      routes.pane_link_cwd(pane),
      routes.is_nvim(pane) and "nvim" or "terminal",
      pane:get_user_vars()[user_vars.open_socket] or "",
    }, "No nvim session found for file link: " .. path_info)
  end

  function routes.open_remote_in_nvim(window, pane, remote_host, path_info)
    -- SSH ControlMaster is the least invasive route when it exists: it does not
    -- depend on visible pane focus or tmux prefix state. Non-OpenSSH transports
    -- may not expose a ControlPath, so keep pane routing as the fallback for
    -- those sessions.
    if routes.open_remote_via_controlmaster(remote_host, path_info) then
      return
    end

    if routes.send_remote_tmux_open(pane, path_info) then
      return
    end

    -- Let the helper use tmux pane APIs for remote SSH panes. Sending prefix
    -- bytes directly through a local tmux pane would be consumed locally
    -- instead of reaching the remote tmux session.
    routes.run_nvim_helper(window, {
      routes.helper_command("nvim-tmux-open"),
      "link",
      path_info,
      "",
      "remote",
      remote_host,
    }, "No nvim session found for " .. remote_host .. ": " .. path_info)
  end

  function routes.open_uri(window, pane, uri)
    local path_info = uri:match("^nvim%-open://(.+)$")
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
  end

  return routes
end

return M
