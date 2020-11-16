use jsonpath::{Match, Step};

use serde_json::Value;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Write;
use std::io::{Write as IOWrite};

use crate::config::MetricType;
use crate::prepare::{
    PreparedLabels,
    PreparedMetric,
    PreparedMetrics,
};

impl PreparedMetrics {
    pub fn process(
        &self, root_metric: &ResolvedMetric, json: &Value, buf: &mut Vec<u8>
    ) {
        let mut stack: Vec<
            (
                std::slice::Iter<PreparedMetric>,
                Option<Vec<(&Value, ResolvedMetric)>>
            )
        > = vec!();
        stack.push((self.iter(), None));
        let mut seen_metrics = HashMap::new();

        // println!("{:?}", json);
        // println!("{:?}", jsonpath::Selector::new("$").unwrap().find(json).map(|v| v.value).collect::<Vec<_>>());

        while let Some((ref mut current_metrics, parent_state)) = stack.last_mut() {
            match current_metrics.next() {
                Some(metric) => {
                    let state = if let Some(parent_state) = parent_state {
                        parent_state.iter()
                            .flat_map(|(parent_json, parent_metric)| {
                                metric.selector.find(parent_json)
                                    .filter_map(|v| {
                                        metric.resolve(&v).map(|m| (v.value, m))
                                    })
                                    .map(move |(v, m)| {
                                        (v, m.merge_with_parent(&parent_metric))
                                    })
                            })
                            .collect::<Vec<_>>()
                    } else {
                        metric.selector.find(json)
                            .filter_map(|v| {
                                metric.resolve(&v).map(|m| (v.value, m))
                            })
                            .map(move |(v, m)| {
                                (v, m.merge_with_parent(root_metric))
                            })
                            .collect::<Vec<_>>()
                    };

                    if metric.metrics.0.is_empty() {
                        // leaf metric
                        // println!("! {}", &metric.selector.expression);
                        // for (json, resolved_metric) in &state {
                        //     println!("  {}", resolved_metric);
                        //     println!("  {:?}", json);
                        // }

                        'metrics_loop: for (json, resolved_metric) in &state {
                            // TODO: apply filters for all values not only leaf
                            let mut _value;
                            let value = if metric.filters.is_empty() {
                                json
                            } else {
                                _value = (*json).clone();
                                for filter in &metric.filters {
                                    match filter.apply(&_value) {
                                        Ok(v) => {
                                            _value = v;
                                        }
                                        Err(_e) => {
                                            // TODO: log error
                                            continue 'metrics_loop;
                                        },
                                    }
                                }
                                &_value
                            };
                            let metric_type = seen_metrics.get(&resolved_metric.name).cloned();
                            let dumped_metric_type = resolved_metric.dump(value, metric_type, buf);
                            if let Some(dumped_metric_type) = dumped_metric_type {
                                if metric_type.is_none() {
                                    seen_metrics.insert(
                                        resolved_metric.name.clone(), dumped_metric_type
                                    );
                                }
                            } else {
                                // TODO: log metric is not dumped
                            }
                        }
                    } else {
                        // parent_metric
                        // println!("> {}", &metric.selector.expression);
                        // for (json, resolved_metric) in &state {
                        //     println!("  {}", resolved_metric);
                        //     println!("  {:?}", json);
                        // }

                        stack.push((metric.metrics.iter(), Some(state)));
                    }
                }
                None => {
                    stack.pop();
                }
            }

        }
    }
}

impl PreparedLabels {
    pub fn resolve(&self, found: &Match) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        for label in &self.labels {
            if let Some(label_value) = (label.value_processor)(found) {
                // Escape label values here so we shouldn't escape them when dumping
                let safe_value = match self.should_escape_label_value(&label_value) {
                    0 => label_value,
                    num_escapes => self.escape_label_value(&label_value, num_escapes),
                };
                labels.insert(
                    label.name.clone(), safe_value
                );
            }
        }
        labels
    }

    fn should_escape_label_value(&self, label_value: &str) -> usize {
        let mut count = 0;
        for c in label_value.chars() {
            if c == '\\' || c == '"' || c == '\n' {
                count += 1;
            }
        }
        count
    }

    fn escape_label_value(&self, label_value: &str, num_escapes: usize) -> String {
        let mut escaped_value = String::with_capacity(label_value.len() + num_escapes * 2);
        for c in label_value.chars() {
            match c {
                '"' => escaped_value.push_str("\\\""),
                '\n' => escaped_value.push_str("\\n"),
                '\\' => escaped_value.push_str("\\\\"),
                c => escaped_value.push(c),
            }
        }
        escaped_value
    }
}

impl PreparedMetric {
    fn resolve(&self, found: &Match) -> Option<ResolvedMetric> {
        let name = match &self.name_processor {
            Some(name_processor) => {
                if let Some(metric_name) = name_processor(found) {
                    metric_name
                } else {
                    return None;
                }
            }
            None => {
                let mut metric_name = String::new();
                for (ix, step) in found.path.iter().enumerate() {
                    if ix > 1 {
                        metric_name.push('_');
                    }
                    match step {
                        Step::Root => continue,
                        Step::Index(ix) => {
                            if write!(&mut metric_name, "{}", ix).is_err() {
                                unreachable!()
                            }
                        },
                        Step::Key(key) => metric_name.push_str(key),
                    }
                }
                metric_name
            }
        };

        Some(ResolvedMetric {
            name,
            metric_type: self.metric_type,
            labels: self.labels.resolve(found),
        })
    }
}

#[derive(Default)]
pub struct ResolvedMetric {
    pub name: String,
    pub metric_type: Option<MetricType>,
    // Use BTreeMap for reproducible tests
    pub labels: BTreeMap<String, String>,
}

impl ResolvedMetric {
    fn merge_with_parent(mut self, parent: &ResolvedMetric) -> Self {
        self.name = if parent.name.is_empty() {
            self.name.clone()
        } else if self.name.is_empty() {
            parent.name.clone()
        } else {
            format!("{}_{}", &parent.name, &self.name)
        };
        for (parent_label_name, parent_label_value) in parent.labels.iter() {
            self.labels.entry(parent_label_name.clone())
                .or_insert(parent_label_value.clone());
        }
        self
    }

    fn dump(
        &self,
        value: &Value,
        seen_metric_type: Option<MetricType>,
        buf: &mut Vec<u8>
    ) -> Option<MetricType> {
        // See: https://prometheus.io/docs/instrumenting/exposition_formats/#comments-help-text-and-type-information

        use MetricType::*;

        let metric_type = match (self.metric_type, seen_metric_type) {
            (Some(mtype), None) | (None, Some(mtype)) => mtype,
            (Some(mtype), Some(seen)) => {
                if mtype != seen {
                    return None;
                }
                seen
            }
            (None, None) => {
                match value {
                    Value::String(_) => Untyped,
                    Value::Number(_) | Value::Bool(_) => Gauge,
                    _ => return None,
                }
            }
        };

        if !self.check_value(value, metric_type) {
            return None;
        }

        if seen_metric_type.is_none() {
            buf.extend(b"# TYPE ");
            buf.extend(self.name.as_bytes());
            match metric_type {
                Gauge => buf.extend(b" gauge\n"),
                Counter => buf.extend(b" counter\n"),
                Untyped => buf.extend(b" untyped\n"),
            }
        }
        self.dump_metric(buf);
        buf.push(b' ');
        self.dump_value(value, buf);
        buf.push(b'\n');
        Some(metric_type)
    }

    fn dump_metric(&self, buf: &mut Vec<u8>) {
        buf.extend(self.name.as_bytes());
        if !self.labels.is_empty() {
            buf.push(b'{');
            for (label_ix, (label_name, label_value)) in self.labels.iter().enumerate() {
                if label_ix > 0 {
                    buf.push(b',');
                }
                buf.extend(label_name.as_bytes());
                buf.push(b'=');
                self.dump_label_value(label_value, buf);
            }
            buf.push(b'}');
        }
    }

    fn dump_label_value(&self, value: &str, buf: &mut Vec<u8>) {
        buf.push(b'"');
        buf.extend(value.as_bytes());
        buf.push(b'"');
    }

    fn check_value(&self, value: &Value, metric_type: MetricType) -> bool {
        use MetricType::*;

        match value {
            Value::Number(_) => true,
            Value::Bool(_) => {
                match metric_type {
                    Gauge | Untyped => true,
                    Counter => false,
                }
            }
            Value::String(_) => {
                match metric_type {
                    Untyped => true,
                    Gauge | Counter => false,
                }
            }
            _ => false,
        }
    }

    fn dump_value(
        &self, value: &Value, buf: &mut Vec<u8>
    ) {
        match value {
            Value::Number(v) => {
                write!(buf, "{}", v).ok();
            }
            Value::Bool(v) if *v => {
                buf.push(b'1')
            }
            Value::Bool(_) => {
                buf.push(b'0')
            }
            Value::String(v) => {
                buf.extend(v.as_bytes());
            }
            _ => {
                unreachable!()
            }
        }
    }
}

impl std::fmt::Display for ResolvedMetric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf = vec!();
        self.dump_metric(&mut buf);
        f.write_str(&String::from_utf8_lossy(&buf))
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Metrics;
    use crate::prepare::PreparedMetrics;
    use super::ResolvedMetric;

    use indoc::indoc;

    use serde_json::Value;
    use serde_yaml;


    fn process_with_config(config: &str, data: &str) -> String {
        let metrics: Metrics = serde_yaml::from_str(config).expect("parse config");
        let prepared_metrics = PreparedMetrics::create_from(&metrics.metrics, None).expect("prepare config");
        let json: Value = serde_json::from_str(data).expect("parse json");

        let ctx = ResolvedMetric::default();
        let mut buf = vec!();
        prepared_metrics.process(&ctx, &json, &mut buf);
        String::from_utf8(buf).expect("utf8 string")
    }

    const DOCS_STATS: &'static str = indoc! {r#"
        {
          "_all": {
            "primaries": {
              "docs": {
                "count": 167172864,
                "deleted": 1345566
              }
            },
            "total": {
              "docs": {
                "count": 334345728,
                "deleted": 2825688
              }
            }
          }
        }
    "#};

    #[test]
    fn test_metric_name_from_selector() {
        let config = indoc! {"
            metrics:
            - path: _all.total
              metrics:
              - path: docs.*
        "};
        assert_eq!(
            process_with_config(config, DOCS_STATS),
            indoc! {r#"
                # TYPE _all_total_docs_count gauge
                _all_total_docs_count 334345728
                # TYPE _all_total_docs_deleted gauge
                _all_total_docs_deleted 2825688
            "#}
        );
    }

    #[test]
    fn test_empty_metric_name() {
        let config = indoc! {"
            metrics:
            - path: _all
              name: ''
              metrics:
              - path: total.docs.*
        "};
        assert_eq!(
            process_with_config(config, DOCS_STATS),
            indoc! {r#"
                # TYPE total_docs_count gauge
                total_docs_count 334345728
                # TYPE total_docs_deleted gauge
                total_docs_deleted 2825688
            "#}
        );
    }

    #[test]
    fn test_path_placeholder() {
        let config = indoc! {"
            metrics:
            - path: _all.*.docs.*
              name: docs_${3}
              labels:
              - name: type
                value: $1
        "};
        assert_eq!(
            process_with_config(config, DOCS_STATS),
            indoc! {r#"
                # TYPE docs_count gauge
                docs_count{type="primaries"} 167172864
                # TYPE docs_deleted gauge
                docs_deleted{type="primaries"} 1345566
                docs_count{type="total"} 334345728
                docs_deleted{type="total"} 2825688
            "#}
        );
    }

    const CLUSTER_HEALTH_STATS: &'static str = r#"
        {
          "cluster_name": "test-cluster",
          "status": "green",
          "timed_out": false,
          "number_of_nodes": 3,
          "number_of_data_nodes": 3,
          "active_primary_shards": 680,
          "active_shards": 1023,
          "relocating_shards": 0,
          "initializing_shards": 0,
          "unassigned_shards": 0,
          "delayed_unassigned_shards": 0,
          "number_of_pending_tasks": 0,
          "number_of_in_flight_fetch": 0,
          "task_max_waiting_in_queue_millis": 0,
          "active_shards_percent_as_number": 100.0
        }
    "#;

    #[test]
    fn test_selecting_root_node() {
        let config = indoc! {"
            metrics:
            # We need to capture a label from a root node
            - path: ''
              labels:
              - name: cluster
                value: ${.cluster_name}
              metrics:
              - path: number_of_nodes
        "};
        assert_eq!(
            process_with_config(config, CLUSTER_HEALTH_STATS),
            indoc! {r#"
                # TYPE number_of_nodes gauge
                number_of_nodes{cluster="test-cluster"} 3
            "#}
        );
    }

    #[test]
    fn test_boolean_value() {
        let config = indoc! {"
            metrics:
            - path: timed_out
        "};
        assert_eq!(
            process_with_config(config, CLUSTER_HEALTH_STATS),
            indoc! {r#"
                # TYPE timed_out gauge
                timed_out 0
            "#}
        );
    }

    #[test]
    fn test_string_value() {
        let config = indoc! {"
            metrics:
            - path: status
        "};
        assert_eq!(
            process_with_config(config, CLUSTER_HEALTH_STATS),
            indoc! {r#"
                # TYPE status untyped
                status green
            "#}
        );
    }

    #[test]
    fn test_simplified_metric() {
        let config = indoc! {"
            metrics:
            - active_shards
            - unassigned_shards
        "};
        assert_eq!(
            process_with_config(config, CLUSTER_HEALTH_STATS),
            indoc! {r#"
                # TYPE active_shards gauge
                active_shards 1023
                # TYPE unassigned_shards gauge
                unassigned_shards 0
            "#}
        );
    }

    const INDICES_STATS: &'static str = r#"
      {
        "_shards": {
          "total": 1023,
          "successful": 1023,
          "failed": 0
        },
        "indices": {
          "catalog": {
            "shards": {
              "0": [
                {
                  "routing": {
                    "primary": false,
                    "node": "kVLufQsXRL-q9l5KN42RIQ"
                  },
                  "docs": {
                    "count": 71317,
                    "deleted": 7724
                  },
                  "search": {
                    "query_total": 8,
                    "query_time_in_millis": 385
                  }
                },
                {
                  "routing": {
                    "primary": true,
                    "node": "g4x8KHe2TS2m7gxlPhwk8g"
                  },
                  "docs": {
                    "count": 71317,
                    "deleted": 9410
                  },
                  "search": {
                    "query_total": 9,
                    "query_time_in_millis": 902
                  }
                }
              ],
              "1": [
                {
                  "routing": {
                    "primary": false,
                    "node": "kVLufQsXRL-q9l5KN42RIQ"
                  },
                  "docs": {
                    "count": 7471,
                    "deleted": 4
                  },
                  "search": {
                    "query_total": 6,
                    "query_time_in_millis": 533
                  }
                },
                {
                  "routing": {
                    "primary": true,
                    "node": "g4x8KHe2TS2m7gxlPhwk8g"
                  },
                  "docs": {
                    "count": 7471,
                    "deleted": 4
                  },
                  "search": {
                    "query_total": 9,
                    "query_time_in_millis": 351
                  }
                }
              ]
            }
          }
        }
      }
    "#;

    #[test]
    fn test_indices_docs() {
        let config = indoc! {"
            metrics:
            - path: _shards.*
              name: shards_$1
            - path: indices.*
              name: indices
              labels:
              - name: index
                value: $1
              metrics:
              - path: shards.*.*
                name: shards
                labels:
                - name: shard
                  value: $1
                - name: node
                  value: ${.routing.node}
                metrics:
                - path: docs.*
                  name: docs_$1
        "};
        assert_eq!(
            process_with_config(config, INDICES_STATS),
            indoc! {"
              # TYPE shards_failed gauge
              shards_failed 0
              # TYPE shards_successful gauge
              shards_successful 1023
              # TYPE shards_total gauge
              shards_total 1023
              # TYPE indices_shards_docs_count gauge
              indices_shards_docs_count{index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"0\"} 71317
              # TYPE indices_shards_docs_deleted gauge
              indices_shards_docs_deleted{index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"0\"} 7724
              indices_shards_docs_count{index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"0\"} 71317
              indices_shards_docs_deleted{index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"0\"} 9410
              indices_shards_docs_count{index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"1\"} 7471
              indices_shards_docs_deleted{index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"1\"} 4
              indices_shards_docs_count{index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"1\"} 7471
              indices_shards_docs_deleted{index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"1\"} 4
            "}
        );
    }

    #[test]
    fn test_indices_search() {
        let config = indoc! {"
            metrics:
            - path: indices.*
              name: indices
              labels:
              - name: index
                value: $1
              metrics:
              - path: shards.*.*
                name: shards
                labels:
                - name: shard
                  value: $1
                - name: node
                  value: ${.routing.node}
                metrics:
                - path: search.*
                  type: counter
                  name: search_$1
        "};
        assert_eq!(
            process_with_config(config, INDICES_STATS),
            indoc! {"
              # TYPE indices_shards_search_query_time_in_millis counter
              indices_shards_search_query_time_in_millis\
                {index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"0\"} 385
              # TYPE indices_shards_search_query_total counter
              indices_shards_search_query_total\
                {index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"0\"} 8
              indices_shards_search_query_time_in_millis\
                {index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"0\"} 902
              indices_shards_search_query_total\
                {index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"0\"} 9
              indices_shards_search_query_time_in_millis\
                {index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"1\"} 533
              indices_shards_search_query_total\
                {index=\"catalog\",node=\"kVLufQsXRL-q9l5KN42RIQ\",shard=\"1\"} 6
              indices_shards_search_query_time_in_millis\
                {index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"1\"} 351
              indices_shards_search_query_total\
                {index=\"catalog\",node=\"g4x8KHe2TS2m7gxlPhwk8g\",shard=\"1\"} 9
            "}
        );
    }

    #[test]
    fn test_multiply_filter() {
        let config = indoc! {"
            metrics:
            - path: query_time_in_millis
              name: query_time_in_seconds
              modifiers:
              - name: mul
                args: 0.001
            - path: index_time_in_millis
              name: index_time_in_seconds
              modifiers:
              - name: div
                args: 1000
        "};
        let json = indoc! {r#"
            {
              "query_time_in_millis": 112,
              "index_time_in_millis": 23.4
            }
        "#};
        assert_eq!(
            process_with_config(config, json),
            indoc! {"
                # TYPE query_time_in_seconds gauge
                query_time_in_seconds 0.112
                # TYPE index_time_in_seconds gauge
                index_time_in_seconds 0.023399999999999997
            "}
        );
    }
}
