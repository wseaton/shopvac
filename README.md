# shopvac


## Example:

To drop all pods that aren't running or pending that were kicked off by the SparkOperator, at least 3 days old (the default timeout):
```sh
shopvac -n spark-namespace -l "sparkoperator.k8s.io/launched-by-spark-operator=true" -f "status.phase!=Running,status.phase!=Pending"
```

### Cluster mode

If a namespace is not provided the tool will run in cluster mode!


## TODO:

* Make this a Controller w/ a CRD, so 'profiles' can be set up for deletions on the cluster.