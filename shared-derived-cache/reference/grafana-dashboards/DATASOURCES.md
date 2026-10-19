<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Dashboard datasource selection

The dashboards use a `datasource` variable to select a Prometheus datasource
available in your Grafana instance. All Prometheus panel queries and the
`namespace` variable use this selection. Grafana's built-in annotation and
Dashboard datasources remain unchanged; the Dashboard datasource reuses results
from other panels.

1. Add a Prometheus datasource in Grafana that contains your OVDC metrics. A
   Prometheus-compatible backend, such as Thanos, can use the same datasource type.
2. Import `main.json`, `read.json`, and `write.json` through Grafana's dashboard
   import screen.
3. Open each imported dashboard and select your metrics backend from its
   `datasource` dropdown. No deployment-specific selection is saved in these
   files; Grafana loads the available Prometheus datasources when the dashboard
   opens and initially selects the first option.
4. Select your deployment's `Namespace` from the populated dropdown. If you want
   to retain your selections, save the dashboard with the current variable values.

With multiple metrics backends, check the `datasource` selection on every
dashboard. If the list is empty, configure a Prometheus datasource first. If
panels remain empty after selecting a datasource and namespace, check that the
backend is receiving OVDC metrics for the selected time range.

See Grafana's [datasource variable documentation](https://grafana.com/docs/grafana/latest/visualizations/dashboards/variables/add-template-variables/#add-a-data-source-variable)
for details.
