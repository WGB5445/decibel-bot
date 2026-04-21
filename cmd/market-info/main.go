package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/bujih/decibel-mm-go/internal/config"
	"github.com/bujih/decibel-mm-go/internal/decibel"
	"github.com/urfave/cli/v2"
)

func main() {
	cli.AppHelpTemplate = `NAME:
   {{.Name}} - {{.Usage}}

USAGE:
   {{.HelpName}} {{if .VisibleFlags}}[全局选项]{{end}} {{if .Commands}}命令 [命令选项]{{end}}
{{if len .Authors}}
AUTHOR:
   {{range .Authors}}{{ . }}{{end}}
   {{end}}{{if .Commands}}
COMMANDS:
{{range .Commands}}{{if not .HideHelp}}   {{join .Names ", "}}{{ "\t" }}{{.Usage}}{{ "\n" }}{{end}}{{end}}{{end}}{{if .VisibleFlags}}
全局选项:
   {{range .VisibleFlags}}{{.}}
   {{end}}{{end}}
`

	app := &cli.App{
		Name:    "market-info",
		Usage:   "查询 Decibel 市场信息和实时价格",
		Version: "1.0.0",
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:    "config",
				Aliases: []string{"c"},
				EnvVars: []string{"DECIBEL_CONFIG"},
				Value:   "configs/config.yaml",
				Usage:   "配置文件路径（默认: configs/config.yaml）",
			},
		},
		Action: runMarketInfo,
	}

	// 覆盖内置的 help 和 version 描述为中文
	cli.HelpFlag = &cli.BoolFlag{
		Name:    "help",
		Aliases: []string{"h"},
		Usage:   "显示帮助信息",
	}
	cli.VersionFlag = &cli.BoolFlag{
		Name:    "version",
		Aliases: []string{"v"},
		Usage:   "显示版本号",
	}

	if err := app.Run(os.Args); err != nil {
		fmt.Fprintf(os.Stderr, "错误: %v\n", err)
		os.Exit(1)
	}
}

func runMarketInfo(c *cli.Context) error {
	cfgPath := c.String("config")
	if cfgPath == "" {
		cfgPath = "configs/config.yaml"
	}

	cfg, err := config.Load(cfgPath)
	if err != nil {
		return fmt.Errorf("加载配置失败: %w", err)
	}

	restBase, _, _ := cfg.BaseURLs()
	client := decibel.NewReadClient(restBase, cfg.Decibel.BearerToken)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	markets, err := client.GetMarkets(ctx)
	if err != nil {
		return fmt.Errorf("获取市场列表失败: %w", err)
	}

	fmt.Printf("%-16s %-50s %s\n", "名称", "市场地址", "精度(价格/数量)")
	fmt.Println("--------------------------------------------------------------------------------")
	for _, m := range markets {
		fmt.Printf("%-16s %-50s %d / %d\n", m.Name, m.MarketAddr, m.PriceDecimals, m.SizeDecimals)
	}

	// Print prices if requested
	prices, err := client.GetPrices(ctx)
	if err == nil {
		fmt.Println("\n实时价格:")
		fmt.Printf("%-16s %-12s %-12s %-12s\n", "名称", "预言机价", "标记价", "中间价")
		fmt.Println("--------------------------------------------------------")
		for _, p := range prices {
			fmt.Printf("%-16s %-12.2f %-12.2f %-12.2f\n", p.Market, p.OraclePx, p.MarkPx, p.MidPx)
		}
	}

	return nil
}
