package clickhouse

import (
	"metaflow/querier/engine/clickhouse/view"
)

type Table struct {
	Value string
}

func (t *Table) Format(m *view.Model) {
	m.AddTable(t.Value)
}
