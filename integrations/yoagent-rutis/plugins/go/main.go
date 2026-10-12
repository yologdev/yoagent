// A yoagent handler written in Go: a policy, a tool, and a slow policy that
// watches its call's context (cancelled when the host gives the call up).
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/arcships/rutis/go/rutis"
)

type Config struct {
	Out string `json:"out"`
}

func record(path, line string) {
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err == nil {
		fmt.Fprintln(f, line)
		f.Close()
	}
}

var Plugin = rutis.Define(rutis.Plugin[Config]{
	Name:   "go-policy",
	Inject: []string{"yoagent"},
	Apply: func(ctx *rutis.Ctx, config Config) error {
		var yoagent rutis.Service
		if err := ctx.Use("yoagent", &yoagent); err != nil {
			return err
		}
		handler := map[string]any{
			"before_tool": func(call context.Context, c map[string]any) (any, error) {
				tool, _ := c["tool"].(string)
				record(config.Out, "go before_tool: "+tool)
				switch tool {
				case "bash":
					return map[string]any{"deny": "no shell from Go"}, nil
				case "slow_gate":
					select {
					case <-call.Done():
						record(config.Out, "go slow_gate: cancelled")
						return nil, call.Err()
					case <-time.After(60 * time.Second):
						return nil, nil
					}
				}
				return nil, nil
			},
			"tools": func(context.Context, map[string]any) (any, error) {
				return []map[string]any{{"name": "go_echo", "description": "echoes from Go"}}, nil
			},
			"call_tool": func(_ context.Context, c map[string]any) (any, error) {
				return fmt.Sprintf("go says %v", c["args"]), nil
			},
		}
		var dispose *rutis.Function
		if err := yoagent.Call(ctx, "register", &dispose, "go-policy", handler); err != nil {
			return err
		}
		record(config.Out, "go registered")
		ctx.Effect(func(c context.Context) error {
			if dispose == nil {
				return nil
			}
			return dispose.Call(c, nil)
		})
		return nil
	},
})

func main() { rutis.Serve(Plugin) }
