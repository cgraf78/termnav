-- Public link rules adapted to WezTerm's hyperlink_rules format.
--
-- The consumer decides where these rules fit in the full WezTerm config, but the
-- reusable token definitions live here with the tmux ctrl-click detectors that
-- recognize the same public terminal shapes.

local M = {}

local public_rules = {
  {
    -- Examples: wss://example.com/socket, vscode://file/home/me/project
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))((?!(?:https?|ftp|file|nvim-open|nvim-remote|lazygit-edit)://)[A-Za-z][A-Za-z0-9+.-]*://[^\s"'`<>│┃║]*[^\s"'`<>.,;:│┃║])(?=$|[\s"'`<>),;])]],
    format = "$1",
  },
  {
    -- Examples: localhost:5173?debug=1, 192.168.1.20:8080/status
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))((?:localhost|host\.docker\.internal|127(?:\.\d{1,3}){3}|10(?:\.\d{1,3}){3}|192\.168(?:\.\d{1,3}){2}|172\.(?:1[6-9]|2\d|3[01])(?:\.\d{1,3}){2}|0\.0\.0\.0|\[::1\]):\d{2,5}(?:[/?#][^\s"'`<>│┃║]*[^\s"'`<>.,;:│┃║])?)(?=$|[\s"'`<>),;])]],
    format = "http://$1",
  },
  {
    -- Examples: www.example.com?x=1, github.com/example/project
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))((?:www\.)[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+(?::\d{2,5})?(?:[/?#][^\s"'`<>│┃║]*[^\s"'`<>.,;:│┃║])?|(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,}(?::\d{2,5}(?:[/?#][^\s"'`<>│┃║]*[^\s"'`<>.,;:│┃║])?|[/?#][^\s"'`<>│┃║]*[^\s"'`<>.,;:│┃║]))(?=$|[\s"'`<>),;])]],
    format = "https://$1",
  },
  {
    -- Example: git@github.com:example/project.git
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))(git@(github\.com|gitlab\.com|bitbucket\.org):([A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+)\.git)(?=$|[\s"'`<>),;])]],
    format = "https://$2/$3",
  },
  {
    -- Example: git@gitlab.com:org/subgroup/project
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))(git@(github\.com|gitlab\.com|bitbucket\.org):([A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+))(?=$|[\s"'`<>),;])]],
    format = "https://$2/$3",
  },
  {
    -- Examples: CVE-2024-12345, cve-2024-12345
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))[Cc][Vv][Ee]-(\d{4})-(\d{4,})(?=$|[\s"'`<>),;])]],
    format = "https://www.cve.org/CVERecord?id=CVE-$1-$2",
  },
  {
    -- Examples: RFC-9110, rfc 9110
    regex = [[(?:^|(?<=[^\w@.+~/'"`-]))([Rr][Ff][Cc][- ](\d{3,5}))(?=$|[\s"'`<>),;])]],
    format = "https://www.rfc-editor.org/rfc/rfc$2",
  },
}

local function copy_rule(rule)
  return {
    regex = rule.regex,
    format = rule.format,
  }
end

-- Return fresh table copies so callers can reorder or mutate their local
-- WezTerm config without changing the shared dependency definitions.
function M.public_link_rules()
  local rules = {}
  for index, rule in ipairs(public_rules) do
    rules[index] = copy_rule(rule)
  end
  return rules
end

-- Append to an existing WezTerm hyperlink_rules table and return it for
-- convenient composition in caller configs.
function M.add_public_link_rules(rules)
  for _, rule in ipairs(public_rules) do
    table.insert(rules, copy_rule(rule))
  end
  return rules
end

return M
