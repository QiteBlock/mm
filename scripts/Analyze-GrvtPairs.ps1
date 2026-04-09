param(
    [string]$MarketDataBaseUrl = "https://market-data.grvt.io",
    [ValidateSet("ticker", "trade_history")]
    [string]$Mode = "trade_history",
    [int]$LookbackMinutes = 1440,
    [int]$TradeHistoryLimit = 500,
    [int]$MaxTradeHistoryPages = 20,
    [int]$TopN = 20,
    [string]$OutCsv = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Convert-GrvtSymbolToInternal {
    param([string]$Symbol)
    return $Symbol.Replace("_Perp", "-P").Replace("_", "/")
}

function Invoke-GrvtPost {
    param(
        [string]$BaseUrl,
        [string]$Path,
        [hashtable]$Body
    )

    return Invoke-RestMethod `
        -Method Post `
        -Uri ($BaseUrl.TrimEnd('/') + $Path) `
        -ContentType "application/json" `
        -Body ($Body | ConvertTo-Json -Compress)
}

function Parse-DecimalOrNull {
    param($Value)

    if ($null -eq $Value) {
        return $null
    }

    $parsed = 0.0
    if ([double]::TryParse($Value.ToString(), [System.Globalization.NumberStyles]::Float, [System.Globalization.CultureInfo]::InvariantCulture, [ref]$parsed)) {
        return $parsed
    }

    return $null
}

function Coalesce-Number {
    param(
        $Value,
        [double]$Default = 0.0
    )

    if ($null -eq $Value) {
        return $Default
    }

    return [double]$Value
}

function Get-OptionalPropertyValue {
    param(
        $Object,
        [string]$Name,
        $Default = $null
    )

    if ($null -eq $Object) {
        return $Default
    }

    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $Default
    }

    return $property.Value
}

function Get-TradeHistoryRows {
    param(
        [string]$BaseUrl,
        [string]$Instrument,
        [string]$StartTimeNs,
        [string]$EndTimeNs,
        [int]$Limit,
        [int]$MaxPages
    )

    $allRows = New-Object System.Collections.Generic.List[object]
    $cursor = ""

    for ($page = 0; $page -lt $MaxPages; $page++) {
        $body = @{
            instrument = $Instrument
            start_time = $StartTimeNs
            end_time = $EndTimeNs
            limit = $Limit
            cursor = $cursor
        }

        $response = Invoke-GrvtPost -BaseUrl $BaseUrl -Path "/full/v1/trade_history" -Body $body
        $pageRows = @(Get-OptionalPropertyValue -Object $response -Name "result" -Default @())

        foreach ($row in $pageRows) {
            [void]$allRows.Add($row)
        }

        $nextCursor = [string](Get-OptionalPropertyValue -Object $response -Name "next" -Default "")
        if ([string]::IsNullOrWhiteSpace($nextCursor)) {
            break
        }

        $cursor = $nextCursor
    }

    return @($allRows)
}

Write-Host "Fetching GRVT active perpetual instruments..."
$instrumentResponse = Invoke-GrvtPost -BaseUrl $MarketDataBaseUrl -Path "/full/v1/all_instruments" -Body @{
    kind = @("PERPETUAL")
    is_active = $true
    limit = 1000
}

$instruments = @($instrumentResponse.result) | Where-Object {
    $_.kind -eq "PERPETUAL" -and ($_.venues -contains "ORDERBOOK")
}

if (-not $instruments) {
    throw "No active GRVT perpetual ORDERBOOK instruments found."
}

$nowNs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() * 1000000L
$lookbackNs = [int64]$LookbackMinutes * 60L * 1000000000L
$startTimeNs = [string]($nowNs - $lookbackNs)
$endTimeNs = [string]$nowNs

if ($Mode -eq "ticker") {
    if ($LookbackMinutes -ne 1440) {
        Write-Warning "GRVT ticker derived stats are fixed to a 24h window. Requested LookbackMinutes=$LookbackMinutes will be ignored."
    }
    Write-Host "Analyzing $($instruments.Count) GRVT perpetual pairs using GRVT ticker derived 24h stats..."
} else {
    Write-Host "Analyzing $($instruments.Count) GRVT perpetual pairs over the last $LookbackMinutes minutes using trade_history..."
}

$rows = foreach ($instrument in $instruments) {
    $ticker = $null
    $tradeRows = @()

    try {
        $tickerResponse = Invoke-GrvtPost -BaseUrl $MarketDataBaseUrl -Path "/full/v1/ticker" -Body @{
            instrument = $instrument.instrument
            derived = $true
        }
        $ticker = $tickerResponse.result
    } catch {
        Write-Warning "ticker failed for $($instrument.instrument): $($_.Exception.Message)"
        continue
    }

    if ($Mode -eq "trade_history") {
        try {
            $tradeRows = Get-TradeHistoryRows `
                -BaseUrl $MarketDataBaseUrl `
                -Instrument $instrument.instrument `
                -StartTimeNs $startTimeNs `
                -EndTimeNs $endTimeNs `
                -Limit $TradeHistoryLimit `
                -MaxPages $MaxTradeHistoryPages
        } catch {
            Write-Warning "trade_history failed for $($instrument.instrument): $($_.Exception.Message)"
            continue
        }
    }

    $bestBid = Parse-DecimalOrNull $ticker.best_bid_price
    $bestAsk = Parse-DecimalOrNull $ticker.best_ask_price
    $markPrice = Parse-DecimalOrNull $ticker.mark_price
    $indexPrice = Parse-DecimalOrNull $ticker.index_price
    $lastPrice = Parse-DecimalOrNull $ticker.last_price
    $buyVolume24hQ = Parse-DecimalOrNull $ticker.buy_volume_24h_q
    $sellVolume24hQ = Parse-DecimalOrNull $ticker.sell_volume_24h_q
    $buyVolume24hB = Parse-DecimalOrNull $ticker.buy_volume_24h_b
    $sellVolume24hB = Parse-DecimalOrNull $ticker.sell_volume_24h_b
    $openInterest = Parse-DecimalOrNull $ticker.open_interest
    $fundingRate = Parse-DecimalOrNull $ticker.funding_rate

    if ($null -eq $bestBid -or $null -eq $bestAsk -or $bestAsk -le $bestBid) {
        continue
    }

    $mid = ($bestBid + $bestAsk) / 2.0
    if ($mid -le 0) {
        continue
    }

    $spreadBps = (($bestAsk - $bestBid) / $mid) * 10000.0

    if ($Mode -eq "trade_history") {
        $tradeCount = 0
        $tradeNotionalUsd = 0.0
        $tradeSizeBase = 0.0
        $takerBuyNotional = 0.0
        $takerSellNotional = 0.0

        foreach ($trade in $tradeRows) {
            $price = Parse-DecimalOrNull $trade.price
            $size = Parse-DecimalOrNull $trade.size
            if ($null -eq $price -or $null -eq $size) {
                continue
            }

            $notional = $price * $size
            $tradeCount += 1
            $tradeNotionalUsd += $notional
            $tradeSizeBase += $size

            if ($trade.is_taker_buyer -eq $true) {
                $takerBuyNotional += $notional
            } else {
                $takerSellNotional += $notional
            }
        }

        $flowImbalance =
            if ($tradeNotionalUsd -gt 0) {
                [Math]::Abs(($takerBuyNotional - $takerSellNotional) / $tradeNotionalUsd)
            } else {
                0.0
            }
    } else {
        $tradeCount = $null
        $tradeNotionalUsd = (Coalesce-Number $buyVolume24hQ) + (Coalesce-Number $sellVolume24hQ)
        $tradeSizeBase = (Coalesce-Number $buyVolume24hB) + (Coalesce-Number $sellVolume24hB)
        $flowImbalance =
            if ($tradeNotionalUsd -gt 0) {
                [Math]::Abs(((Coalesce-Number $buyVolume24hQ) - (Coalesce-Number $sellVolume24hQ)) / $tradeNotionalUsd)
            } else {
                0.0
            }
    }

    [pscustomobject]@{
        Symbol = Convert-GrvtSymbolToInternal $instrument.instrument
        GrvtInstrument = $instrument.instrument
        AvgSpreadBps = [Math]::Round($spreadBps, 4)
        TradeCount = $tradeCount
        TradeNotionalUsd = [Math]::Round($tradeNotionalUsd, 2)
        TradeSizeBase = [Math]::Round($tradeSizeBase, 6)
        FlowImbalance = [Math]::Round($flowImbalance, 4)
        LastBid = $bestBid
        LastAsk = $bestAsk
        MarkPrice = $markPrice
        IndexPrice = $indexPrice
        LastTradePrice = $lastPrice
        OpenInterest = $openInterest
        FundingRate = $fundingRate
        MinSize = $instrument.min_size
        MinNotional = $instrument.min_notional
        TickSize = $instrument.tick_size
    }
}

$validRows = @($rows | Where-Object { $null -ne $_.AvgSpreadBps })
if (-not $validRows) {
    throw "No market-data samples collected from GRVT $Mode endpoints."
}

$spreadValues = $validRows | ForEach-Object { [double]$_.AvgSpreadBps }
$notionalValues = $validRows | ForEach-Object { [double]$_.TradeNotionalUsd }
$openInterestValues = $validRows | ForEach-Object { Coalesce-Number $_.OpenInterest }
$flowImbalanceValues = $validRows | ForEach-Object { [double]$_.FlowImbalance }

$minSpread = ($spreadValues | Measure-Object -Minimum).Minimum
$maxSpread = ($spreadValues | Measure-Object -Maximum).Maximum
$minNotional = ($notionalValues | Measure-Object -Minimum).Minimum
$maxNotional = ($notionalValues | Measure-Object -Maximum).Maximum
$minOpenInterest = ($openInterestValues | Measure-Object -Minimum).Minimum
$maxOpenInterest = ($openInterestValues | Measure-Object -Maximum).Maximum
$minFlowImbalance = ($flowImbalanceValues | Measure-Object -Minimum).Minimum
$maxFlowImbalance = ($flowImbalanceValues | Measure-Object -Maximum).Maximum

$scored = $validRows | ForEach-Object {
    $spreadNorm =
        if ($maxSpread -le $minSpread) { 1.0 }
        else { 1.0 - (($_.AvgSpreadBps - $minSpread) / ($maxSpread - $minSpread)) }

    $notionalNorm =
        if ($maxNotional -le $minNotional) { 0.0 }
        else { ($_.TradeNotionalUsd - $minNotional) / ($maxNotional - $minNotional) }

    $openInterestNorm =
        if ($maxOpenInterest -le $minOpenInterest) { 0.0 }
        else { ((Coalesce-Number $_.OpenInterest) - $minOpenInterest) / ($maxOpenInterest - $minOpenInterest) }

    $flowBalanceNorm =
        if ($maxFlowImbalance -le $minFlowImbalance) { 1.0 }
        else { 1.0 - (($_.FlowImbalance - $minFlowImbalance) / ($maxFlowImbalance - $minFlowImbalance)) }

    $score = 100.0 * (
        (0.45 * $spreadNorm) +
        (0.30 * $notionalNorm) +
        (0.15 * $openInterestNorm) +
        (0.10 * $flowBalanceNorm)
    )

    $recommendation =
        if ($score -ge 70) { "strong candidate" }
        elseif ($score -ge 50) { "worth testing" }
        elseif ($score -ge 30) { "maybe with tuning" }
        else { "poor current fit" }

    [pscustomobject]@{
        Symbol = $_.Symbol
        Score = [Math]::Round($score, 2)
        Recommendation = $recommendation
        AvgSpreadBps = $_.AvgSpreadBps
        TradeCount = $_.TradeCount
        TradeNotionalUsd24h = $_.TradeNotionalUsd
        OpenInterest = $_.OpenInterest
        FlowImbalance = $_.FlowImbalance
        LastPrice = if ($null -ne $_.LastTradePrice) { $_.LastTradePrice } else { $_.MarkPrice }
        MinNotional = $_.MinNotional
        TickSize = $_.TickSize
        FundingRate = $_.FundingRate
    }
}

$ranked = $scored | Sort-Object @{ Expression = "Score"; Descending = $true }, @{ Expression = "TradeNotionalUsd24h"; Descending = $true }

if ($OutCsv) {
    $ranked | Export-Csv -LiteralPath $OutCsv -NoTypeInformation
}

$ranked | Select-Object -First $TopN | Format-Table Symbol, Score, Recommendation, AvgSpreadBps, TradeCount, TradeNotionalUsd24h, OpenInterest, FlowImbalance, LastPrice, FundingRate, MinNotional -AutoSize

Write-Host ""
Write-Host "Scoring notes:"
if ($Mode -eq "trade_history") {
    Write-Host "- Uses GRVT REST endpoints: all_instruments, ticker, and trade_history."
    Write-Host "- TradeCount / TradeNotionalUsd24h are computed from the requested trade_history lookback window."
} else {
    Write-Host "- Uses GRVT REST endpoints: all_instruments and ticker with derived 24h fields."
    Write-Host "- TradeNotionalUsd24h comes from GRVT derived ticker fields."
}
Write-Host "- Higher score means tighter spread, stronger notional, deeper open interest, and more balanced taker flow."
Write-Host "- This is a market-selection screen, not a profitability guarantee."
