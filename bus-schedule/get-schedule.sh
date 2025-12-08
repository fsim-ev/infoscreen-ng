#!/usr/bin/env bash

# SPDX-License-Identifier: The Unlicense

BUS_STOP=$1
LIMIT=100

set -euo pipefail

curl -v "https://mobile.defas-fgi.de/beg/json/XML_DM_REQUEST" \
  --silent \
  -d "outputFormat=JSON" \
  -d "language=de" \
  -d "stateless=1" \
  -d "type_dm=stop" \
  -d "name_dm=$BUS_STOP" \
  -d "useRealtime=1" \
  -d "mode=direct" \
  -d "mergeDep=1" \
  -d "limit=$LIMIT" \
  -d "deleteAssignedStops_dm=1" \
  | jless
  exit
  echo | jq .departureList \
  | jless
  exit
  echo lol | jq '
def unix($x): if ($x | type) != "null" then $x | (.year + "-" + .month  + "-" + .day + "T" + .hour + ":" + .minute + ":00Z") | fromdate else $x end;

.departureList | [
  .[] | {
    time: unix(.dateTime),
    real_time: unix(.realDateTime),
    remaining: .countdown,
    route: .servingLine.symbol,
    final_stop: .servingLine.direction,
    local_stop: .stopName,
    into_city: (.servingLine.liErgRiProj.direction == "R")
  }
]'
