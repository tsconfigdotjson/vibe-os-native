#!/bin/sh
# A stand-in for Pi (vibe_env::testing::PI_STAND_IN). It takes Pi's arguments
# and writes a session as Pi does: `<time>_<id>.jsonl` directly in
# PI_CODING_AGENT_SESSION_DIR, from the first user message on, so a session
# nobody has asked anything has no file. The same --session-id finds the same file, so
# one flag starts and resumes. What it was started with goes beside its
# sessions, in `<dir>.args` and `<dir>.env`, for tests to look at. A model whose
# name starts `no-such` is one it won't take, so it says so and exits, as Pi does.
#
# The prompt's words:
#   wait N     takes N seconds to get going, with nothing on disk at all
#   think N    takes N seconds over its answer, its prompt on disk and no more
#   nologin    fails before it writes anything, showing why only on its screen
#   sayerror   answers with what reads just like that error
#   /anything  is Pi's to deal with: it writes nothing, and shows nothing here
#   tool N     runs a tool for N seconds: a long, silent tool call
#   limit      stops at a usage limit
#   error      stops at an API error
#   anything else answers "Working on: <prompt>"
# It prints "Ready." first, and answers each line it's sent as a turn. Its screen
# is drawn as Pi's is: what was said, then an editor between two rules and a
# footer, and its spinner set in a rule while a turn runs.
if [ "$1" = auth ]; then
  # `pi auth check --model <model> --json`, from its login file.
  model=
  while [ $# -gt 0 ]; do
    case $1 in --model) model=$2; shift 2 ;; *) shift ;; esac
  done
  provider=${model%%/*}
  # As Pi says of a model it doesn't know, loading no extensions for this, and
  # of every bare id, which this command doesn't resolve.
  case $model in */*) ;; *) provider=unknown ;; esac
  case $provider in
    unknown)
      printf '{"status":"invalid","provider":"%s","reason":"invalid_state"}\n' "$model"
      exit 2
      ;;
  esac
  if grep -q "\"$provider\"" "${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/auth.json" 2>/dev/null; then
    printf '{"status":"ready","provider":"%s","authType":"api_key"}\n' "$provider"
    exit 0
  fi
  printf '{"status":"not_ready","provider":"%s","reason":"credentials_not_configured"}\n' "$provider"
  exit 1
fi
if [ "$1" = --mode ] && [ "$2" = rpc ]; then
  # What vibed runs for Pi's list and its sign-in (`vibe_env::pi_setup`), speaking
  # as `vibe-setup.ts` makes Pi speak, with the login it's given written as Pi
  # would. What it was started with goes in the agent dir, for tests to look at.
  dir=${PI_CODING_AGENT_DIR:?}
  printf '%s\n' "$@" >"$dir/rpc.args"
  env >"$dir/rpc.env"
  pwd -P >"$dir/rpc.cwd"
  say() { printf '{"type":"extension_ui_request","id":"n","method":"notify","message":"%s"}\n' "$(printf '%s' "$1" | sed 's/"/\\"/g')"; }
  # ask ID JSON: a question, and leaves its answer in $answer. No answer at all is
  # being cancelled, or told it's over.
  ask() {
    printf '{"type":"extension_ui_request","id":"%s","method":"input","title":"%s","placeholder":""}\n' \
      "$1" "$(printf '%s' "$2" | sed 's/"/\\"/g')"
    IFS= read -r reply || exit 0
    answer=$(printf '%s' "$reply" | sed -n 's/.*"value":"\(.*\)"}$/\1/p')
    say '{"vibe":"prompt_over"}'
  }
  # sign_in PROVIDER KIND VALUE: what Pi's own store would then hold.
  sign_in() {
    if [ -f "$dir/auth.json" ]; then
      rest=$(sed -e 's/^{//' -e 's/}$//' "$dir/auth.json")
    else
      rest=
    fi
    [ -n "$rest" ] && rest=",$rest"
    case $2 in
      oauth) entry=$(printf '{"type":"oauth","access":"%s","refresh":"r","expires":4102444800000}' "$3") ;;
      odd) entry='{"type":"api_key","key":"k","more":1}' ;;
      *) entry=$(printf '{"type":"api_key","key":"%s"}' "$3") ;;
    esac
    printf '{"%s":%s%s}' "$1" "$entry" "$rest" >"$dir/auth.json.new"
    chmod 600 "$dir/auth.json.new"
    mv "$dir/auth.json.new" "$dir/auth.json"
  }
  has() { grep -q "\"$1\"" "$dir/auth.json" 2>/dev/null && echo true || echo false; }
  IFS= read -r line || exit 0
  case $line in
    *'/vibe-catalog'*)
      opencode_key=false
      [ -n "$OPENCODE_API_KEY" ] && opencode_key=true
      opencode=$(has opencode)
      [ "$opencode_key" = true ] && opencode=true
      say "{\"vibe\":\"catalog\",\"providers\":[\
{\"id\":\"anthropic\",\"name\":\"Anthropic\",\"oauth\":true,\"api_key\":true,\"configured\":$(has anthropic),\"models\":[{\"id\":\"claude-sonnet-4\",\"name\":\"Claude Sonnet 4\"}]},\
{\"id\":\"openai\",\"name\":\"OpenAI\",\"oauth\":true,\"oauth_label\":\"Sign in with ChatGPT\",\"api_key\":true,\"configured\":$(has openai),\"models\":[{\"id\":\"gpt-5\",\"name\":\"GPT-5\"},{\"id\":\"gpt-5-mini\",\"name\":\"gpt-5-mini\"}]},\
{\"id\":\"opencode\",\"name\":\"OpenCode Zen\",\"oauth\":false,\"api_key\":true,\"configured\":$opencode,\"models\":[{\"id\":\"deepseek-v4.1-flash\",\"name\":\"DeepSeek V4.1 Flash\"},{\"id\":\"gpt-5\",\"name\":\"GPT-5\"},{\"id\":\"has space\",\"name\":\"Not one to give back\"}]},\
{\"id\":\"amazon-bedrock\",\"name\":\"Amazon Bedrock\",\"oauth\":false,\"api_key\":true,\"configured\":false,\"models\":[]}\
]}"
      while IFS= read -r line; do :; done
      ;;
    *'/vibe-login '*)
      words=${line#*/vibe-login }
      provider=${words%% *}
      method=${words#* }
      method=${method%%\"*}
      case $provider in
        no-such) say '{"vibe":"failed","message":"Unknown provider: no-such"}' ;;
        goes-away) exit 3 ;;
        slow)
          # Never answers, so it can be given up on, as a browser never came back.
          say '{"vibe":"event","type":"progress","message":"Waiting for your browser"}'
          while IFS= read -r line; do :; done
          ;;
        *)
          case $method in
            api_key)
              ask p1 '{"vibe":"prompt","type":"secret","message":"API key"}'
              if [ "$answer" = refused ]; then
                say '{"vibe":"failed","message":"That key was refused"}'
              elif [ "$answer" = odd-shape ]; then
                sign_in "$provider" odd
                say '{"vibe":"done"}'
              else
                sign_in "$provider" api_key "$answer"
                say '{"vibe":"event","type":"progress","message":"Getting its models…"}'
                say '{"vibe":"done"}'
              fi
              ;;
            *)
              ask p1 '{"vibe":"prompt","type":"select","message":"Select login method:","options":[{"id":"browser","label":"Browser login (default)"},{"id":"device_code","label":"Device code login (headless)"}]}'
              if [ "$answer" = device_code ]; then
                ask p2 '{"vibe":"prompt","type":"text","message":"Enterprise domain (blank for none)","placeholder":"company.example"}'
                say '{"vibe":"event","type":"device_code","userCode":"ABCD-1234","verificationUri":"https://auth.example/device?user_code=ABCD-1234","intervalSeconds":5,"expiresInSeconds":600}'
                sign_in "$provider" oauth device-token
                say '{"vibe":"done"}'
              else
                say '{"vibe":"event","type":"progress","message":"Listening for the callback"}'
                port=$(cat "$dir/redirect-port" 2>/dev/null || echo 53692)
                say '{"vibe":"event","type":"auth_url","url":"https://claude.ai/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A'"$port"'%2Fcallback&state=the-state","instructions":"Complete sign-in in your browser."}'
                ask p3 '{"vibe":"prompt","type":"manual_code","message":"Paste the redirect URL here:","placeholder":"http://localhost/callback"}'
                case $answer in
                  *code=good*)
                    sign_in "$provider" oauth "$answer"
                    say '{"vibe":"done"}'
                    ;;
                  *) say '{"vibe":"failed","message":"State mismatch"}' ;;
                esac
              fi
              ;;
          esac
          ;;
      esac
      ;;
  esac
  exit 0
fi
id= prompt= extension=
# Whole or not at all, as a test reads them as soon as they're there.
printf '%s\n' "$@" >"$PI_CODING_AGENT_SESSION_DIR.args.new"
mv "$PI_CODING_AGENT_SESSION_DIR.args.new" "$PI_CODING_AGENT_SESSION_DIR.args"
env >"$PI_CODING_AGENT_SESSION_DIR.env.new"
mv "$PI_CODING_AGENT_SESSION_DIR.env.new" "$PI_CODING_AGENT_SESSION_DIR.env"
while [ $# -gt 0 ]; do
  case $1 in
    --session-id) id=$2; shift 2 ;;
    -e) extension=$2; shift 2 ;;
    --model)
      # As Pi does with one it won't take: says why, and draws nothing.
      case $2 in no-such*)
        printf 'Error: Model "%s" not found. Use --list-models to see available models.\n' "$2" >&2
        exit 1
      esac
      shift 2 ;;
    --thinking) shift 2 ;;
    --approve) shift ;;
    --) shift; prompt=$*; break ;;
    *) shift ;;
  esac
done
cwd=$(pwd -P)
dir=$PI_CODING_AGENT_SESSION_DIR
mkdir -p "$dir"
transcript=
for f in "$dir"/*_"$id".jsonl; do
  [ -f "$f" ] && transcript=$f
done
seq=0 parent=null who=$$ began=
stamp() { printf '%s.%03dZ' "$(date -u +%Y-%m-%dT%H:%M:%S)" $((seq % 1000)); }
# entry <fields>: one line of the session, on the entry before it.
entry() {
  seq=$((seq+1))
  printf '{"id":"%s-%s","parentId":%s,"timestamp":"%s",%s}\n' \
    "$who" "$seq" "$parent" "$(stamp)" "$1" >>"$transcript"
  parent="\"$who-$seq\""
}
json() { printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"; }
usage='"usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}'
# flush: Pi's file appears with the first user message, the entries so far in it.
pending=
flush() {
  if [ -z "$transcript" ]; then
    transcript="$dir/$(date -u +%Y-%m-%dT%H-%M-%S-000Z)_$id.jsonl"
    printf '{"type":"session","version":3,"id":"%s","timestamp":"%s","cwd":"%s"}\n' \
      "$id" "$(stamp)" "$cwd" >>"$transcript"
    entry '"type":"model_change","provider":"mock","modelId":"mock-model"'
  fi
  if [ -n "$pending" ]; then
    entry "\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":$(json "$pending")}],\"timestamp\":1}"
    pending=
  fi
}
user() { pending=$1; flush; }
assistant() {
  flush
  entry "\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":$1,\"api\":\"anthropic-messages\",\"provider\":\"mock\",\"model\":\"mock-model\",$usage,\"stopReason\":\"$2\",\"timestamp\":1$3}"
}
reply() { assistant "[{\"type\":\"text\",\"text\":$(json "$1")}]" stop; echo "$1"; }
# working N: N seconds with its spinner up, which then goes.
working() {
  printf '── ⠋ Working ─────────────\n'
  sleep "$1"
  printf '\033[1A\033[2K'
}
# editor: its editor and footer, with the cursor left in it, for what's sent to
# be echoed there. unedit takes them back, once a line has been sent.
rule='──────────────────────────'
editor() {
  printf '\n%s\n> \n%s\n~/w  mock/mock-model  0%%\n\033[3A\033[2C' "$rule" "$rule"
}
unedit() { printf '\033[3A\033[J'; }
turn() {
  words=" $1 "
  case $1 in /*) return ;; esac
  printf ' %s\n\n' "$1"
  case $words in *" wait "*)
    n=${words#* wait }; working "${n%% *}"
  esac
  case $words in *" nologin "*)
    printf 'Error: No API key found for mock.\n\nUse /login to log into a provider via OAuth or API key. See:\n  /opt/vibeos/pi/docs/providers.md\n  /opt/vibeos/pi/docs/models.md\n'
    return
  esac
  user "$1"
  case $words in *" think "*)
    n=${words#* think }; working "${n%% *}"
  esac
  case $words in
    *" tool "*)
      n=${words#* tool }; n=${n%% *}
      assistant "[{\"type\":\"toolCall\",\"id\":\"t-$seq\",\"name\":\"bash\",\"arguments\":{\"command\":\"sleep $n\"}}]" toolUse
      echo "Running a tool for ${n}s."
      working "$n"
      entry "\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"t-$seq\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}],\"isError\":false,\"timestamp\":1}"
      reply "The tool ran."
      ;;
    *" limit "*)
      assistant '[]' error ',"errorMessage":"429 {\"error\":{\"message\":\"You'"'"'ve hit your 5-hour limit · resets 3:50am (UTC)\",\"type\":\"rate_limit_error\"},\"type\":\"error\"}"'
      echo "Stopped at a usage limit."
      ;;
    *" error "*)
      assistant '[]' error ',"errorMessage":"500 upstream is unwell"'
      echo "Stopped at an API error."
      ;;
    *" sayerror "*) reply "Error: No API key found for stripe." ;;
    *) reply "Working on: $1" ;;
  esac
}
if [ -n "$transcript" ]; then echo "Resumed $id."; fi
echo "Ready."
if [ -n "$prompt" ]; then turn "$prompt"; fi
editor
while IFS= read -r line; do
  unedit
  turn "$line"
  editor
done
